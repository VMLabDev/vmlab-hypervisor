// Oracle: the Sail RISC-V C++ model behind a C ABI for lockstep testing.
//
// One OracleImpl wraps one hart::Model. The generated model routes every
// memory access through the free functions emulator_{read,write}_mem*,
// which the toolchain weakifies in the Sail runtime (see
// toolchain/sail/patches/) so the strong definitions below can forward
// them to host callbacks registered through the C ABI.
//
// The init/step call sequence mirrors sail-riscv's c_emulator
// (preinit_model/init_model/run_sail in riscv_sim.cpp); the platform
// virtuals mirror riscv_model_impl.cpp. Both are BSD-2-Clause; see
// THIRD_PARTY_NOTICES.

#include "oracle.h"

#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "sail.h"
#include "sail_config.h"
#include "sail_riscv_model.h"

#ifndef SAIL_ORACLE_SAIL_VERSION
#define SAIL_ORACLE_SAIL_VERSION "unknown"
#endif
#ifndef SAIL_ORACLE_MODEL_COMMIT
#define SAIL_ORACLE_MODEL_COMMIT "unknown"
#endif

namespace {

class OracleImpl final : private hart::Model {
public:
  OracleImpl() = default;
  ~OracleImpl() { model_fini(); }

  bool init(const char *config_path, uint64_t reset_pc) {
    sail_config_set_file(config_path);

    // Reservation semantics the model expects the platform to configure
    // (mirrors ModelImpl::init_platform_constants).
    const uint64_t set_exp = config_u64({"platform", "reservation", "reservation_set_size_exp"});
    m_reservation_set_addr_mask = ~((UINT64_C(1) << set_exp) - 1);
    m_reservation_require_exact_addr = config_bool({"platform", "reservation", "require_exact_reservation_addr"});
    m_reservation_invalidate_on_same_hart_store = config_bool({"platform", "reservation", "invalidate_on_same_hart_store"});

    model_init();
    if (!zconfig_is_valid(UNIT)) {
      fprintf(stderr, "oracle: model configuration '%s' is invalid\n", config_path);
      return false;
    }

    // Mirrors ModelImpl::init_sail: the reset PC must be set before
    // zinit_model because reset happens inside it.
    zset_pc_reset_address(reset_pc);
    zinit_model(config_path);
    zinit_boot_requirements(UNIT);
    return true;
  }

  // ---- stepping ----

  bool step(uint64_t step_no) {
    sail_int sail_step;
    CREATE(sail_int)(&sail_step);
    CONVERT_OF(sail_int, mach_int)(&sail_step, static_cast<mach_int>(step_no));
    m_trap_seen = false;
    const bool waiting = ztry_step(sail_step, /*exit_wait=*/true);
    KILL(sail_int)(&sail_step);
    // have_exception covers model-internal failures; m_trap_seen is set by
    // trap_callback for architectural exceptions and interrupts.
    m_trapped = have_exception || m_trap_seen;
    return waiting;
  }

  void tick_clock() { ztick_clock(UNIT); }

  // ---- state access ----

  uint64_t xlen() const { return static_cast<uint64_t>(zxlen); }

  uint64_t pc() const { return zPC.bits; }
  void set_pc(uint64_t val) {
    sbits sail_val;
    sail_val.len = zPC.len;
    sail_val.bits = val;
    (void)zset_next_pc(sail_val);
  }

  uint64_t xreg(unsigned reg) {
    const sbits val = zrX(static_cast<int64_t>(reg));
    return val.bits;
  }
  void set_xreg(unsigned reg, uint64_t val) {
    sbits sail_val;
    sail_val.len = static_cast<uint64_t>(zxlen);
    sail_val.bits = val;
    (void)zwX(static_cast<int64_t>(reg), sail_val);
  }

  uint64_t freg(unsigned reg) {
    const sbits val = zrF(static_cast<int64_t>(reg));
    return val.bits;
  }
  void set_freg(unsigned reg, uint64_t val) {
    sbits sail_val;
    sail_val.len = static_cast<uint64_t>(zflen);
    sail_val.bits = val;
    (void)zwF(static_cast<int64_t>(reg), sail_val);
  }

  bool csr_readable(uint16_t addr) { return zis_CSR_accessible(addr, hart::zMachine, hart::zCSRRead); }
  bool csr_writable(uint16_t addr) { return zis_CSR_accessible(addr, hart::zMachine, hart::zCSRWrite); }

  uint64_t get_csr(uint16_t addr) { return zread_CSR(addr).bits; }

  bool set_csr(uint16_t addr, uint64_t val) {
    if (!csr_writable(addr)) {
      return false;
    }
    sbits sail_val;
    sail_val.len = static_cast<uint64_t>(zxlen);
    sail_val.bits = val;
    struct hart::zresultzIbzCuzK result = {};
    zwrite_CSR(&result, addr, sail_val);
    const bool ok = result.kind == hart::Kind_zOkzIbzCuzK;
    if (ok) {
      KILL(lbits)(&result.variants.zOkzIbzCuzK);
    }
    return ok;
  }

  const std::vector<uint16_t> &csr_list() {
    if (!m_csrs_scanned) {
      for (uint32_t addr = 0; addr < 0x1000; addr++) {
        if (csr_readable(static_cast<uint16_t>(addr))) {
          m_csrs.push_back(static_cast<uint16_t>(addr));
        }
      }
      m_csrs_scanned = true;
    }
    return m_csrs;
  }

  const char *isa_string() {
    if (m_isa.empty()) {
      sail_string str = nullptr;
      CREATE(sail_string)(&str);
      zgenerate_canonical_isa_string(&str, UNIT);
      m_isa = str;
      KILL(sail_string)(&str);
    }
    return m_isa.c_str();
  }

  // ---- host hooks (set through the C ABI) ----

  oracle_read_mem_fn read_mem = nullptr;
  oracle_write_mem_fn write_mem = nullptr;
  void *mem_ctx = nullptr;
  oracle_trap_fn trap = nullptr;
  oracle_term_fn term = nullptr;
  void *obs_ctx = nullptr;

  bool trapped() const { return m_trapped; }

  void trap_info(int *is_interrupt, uint64_t *cause) const {
    *is_interrupt = m_trap_is_interrupt;
    *cause = m_trap_cause;
  }

  // The host hooks above are public: the memory seam reads them.

private:
  // ---- configuration helpers (shape from c_emulator/config_utils.cpp) ----

  static uint64_t config_u64(std::initializer_list<const char *> keypath) {
    sail_config_json json = sail_config_get(keypath.size(), keypath.begin());
    if (json == nullptr) {
      return 0;
    }
    if (sail_config_is_int(json)) {
      sail_int n;
      CREATE(sail_int)(&n);
      sail_config_unwrap_int(&n, json);
      const uint64_t v = sail_int_get_ui(n);
      KILL(sail_int)(&n);
      return v;
    }
    if (sail_config_is_bits(json)) {
      lbits bits;
      CREATE(lbits)(&bits);
      sail_config_unwrap_bits(&bits, json);
      uint64_t v = 0;
      if (bits.len <= 64) {
        // Second parameter is unused; see rems-project/sail#1429.
        v = CONVERT_OF(fbits, lbits)(bits, false);
      } else {
        fprintf(stderr, "oracle: %" PRIu64 "-bit config value does not fit in 64 bits; assuming 0\n", bits.len);
      }
      KILL(lbits)(&bits);
      return v;
    }
    return 0;
  }

  static bool config_bool(std::initializer_list<const char *> keypath) {
    sail_config_json json = sail_config_get(keypath.size(), keypath.begin());
    if (json != nullptr && sail_config_is_bool(json)) {
      return sail_config_unwrap_bool(json);
    }
    return false;
  }

  // ---- platform callbacks (subset of PlatformInterface; the rest keep
  // the no-op defaults from c_emulator/riscv_platform_if.cpp) ----

  unit trap_callback(bool is_interrupt, fbits cause) override {
    m_trap_seen = true;
    m_trap_is_interrupt = is_interrupt;
    m_trap_cause = cause;
    if (trap != nullptr) {
      trap(obs_ctx, is_interrupt ? 1 : 0, cause);
    }
    return UNIT;
  }

  unit plat_term_write(mach_bits c) override {
    if (term != nullptr) {
      const char byte = static_cast<char>(c);
      term(obs_ctx, &byte, 1);
    }
    return UNIT;
  }

  // Deterministic entropy for the scalar cryptography extension: a fixed
  // LCG, so oracle runs are reproducible.
  mach_bits plat_get_16_random_bits(unit) override {
    m_entropy = m_entropy * UINT64_C(6364136223846793005) + UINT64_C(1442695040888963407);
    return (m_entropy >> 33) & 0xFFFF;
  }

  bool sys_enable_experimental_extensions(unit) override { return false; }

  // Reservation tracking (mirrors ModelImpl). Store-Conditionals fail
  // only when no reservation is live or the address left the set.
  unit load_reservation(sbits addr, uint64_t width) override {
    m_reservation_addr = addr.bits;
    m_reservation = addr.bits & m_reservation_set_addr_mask;
    m_reservation_valid = true;
    (void)width;
    return UNIT;
  }
  bool match_reservation(sbits addr) override {
    return m_reservation_valid &&
           (m_reservation_require_exact_addr ? (addr.bits == m_reservation_addr)
                                             : (m_reservation & m_reservation_set_addr_mask) ==
                                                   (addr.bits & m_reservation_set_addr_mask));
  }
  unit cancel_reservation(unit) override {
    m_reservation_valid = false;
    return UNIT;
  }
  bool valid_reservation(unit) override { return m_reservation_valid; }

  uint64_t m_reservation = 0;
  uint64_t m_reservation_addr = 0;
  bool m_reservation_valid = false;
  uint64_t m_reservation_set_addr_mask = 0;
  bool m_reservation_require_exact_addr = false;
  bool m_reservation_invalidate_on_same_hart_store = false;

  uint64_t m_entropy = UINT64_C(0x5EED);
  bool m_trapped = false;
  bool m_trap_seen = false;
  bool m_trap_is_interrupt = false;
  uint64_t m_trap_cause = 0;

  std::vector<uint16_t> m_csrs;
  bool m_csrs_scanned = false;
  std::string m_isa;
};

// The single active oracle. The Sail runtime's memory seam is a set of
// process-global free functions, so one oracle at a time.
OracleImpl *g_active = nullptr;

const char *mem_kind_name(int kind) {
  switch (kind) {
  case ORACLE_MEM_READ:
    return "read";
  case ORACLE_MEM_IFETCH:
    return "ifetch";
  case ORACLE_MEM_READ_EXCL:
    return "read-exclusive";
  case ORACLE_MEM_WRITE:
    return "write";
  case ORACLE_MEM_WRITE_EXCL:
    return "write-exclusive";
  default:
    return "?";
  }
}

// Reads route through the host callback into `data` as a little-endian
// mpz. A failing read yields 0xff fill (the architectural "garbage"
// answer); the model cannot express a read fault at this seam.
void seam_read(lbits *data, uint64_t addr, uint64_t len, int kind) {
  uint8_t buf[64];
  if (len > sizeof(buf)) {
    // No architectural RISC-V access exceeds 64 bytes (the model splits
    // wider vector accesses); refuse rather than corrupt state.
    fprintf(stderr, "oracle: %s of %" PRIu64 " bytes at 0x%016" PRIx64 " is too large\n", mem_kind_name(kind), len, addr);
    exit(86);
  }
  const int rc = (g_active != nullptr && g_active->read_mem != nullptr)
                     ? g_active->read_mem(g_active->mem_ctx, addr, buf, len, kind)
                     : -1;
  if (rc != 0) {
    if (kind == ORACLE_MEM_IFETCH) {
      // A failed fetch would execute 0xffff, an illegal instruction loop;
      // abort loudly instead so the harness notices a broken memory image.
      fprintf(stderr, "oracle: host ifetch of %" PRIu64 " bytes at 0x%016" PRIx64 " failed\n", len, addr);
      exit(87);
    }
    memset(buf, 0xff, len);
  }
  if (data != nullptr && data->bits != nullptr) {
    mpz_set_ui(*data->bits, 0);
    if (rc == 0) {
      mpz_import(*data->bits, len, /*order=*/-1, /*size=*/1, /*endian=*/0, /*nails=*/0, buf);
    } else {
      mpz_import(*data->bits, len, -1, 1, 0, 0, buf); // 0xff fill from above
    }
    data->len = len * 8;
  }
}

// Writes export the mpz value little-endian and forward it; a nonzero
// return makes the access fail, which the model reports as an
// access-fault exception.
bool seam_write(uint64_t addr, uint64_t len, const lbits data, int kind) {
  uint8_t buf[64];
  if (len > sizeof(buf)) {
    fprintf(stderr, "oracle: %s of %" PRIu64 " bytes at 0x%016" PRIx64 " is too large\n", mem_kind_name(kind), len, addr);
    exit(86);
  }
  memset(buf, 0, len);
  if (data.bits != nullptr && *data.bits != nullptr) {
    mpz_export(buf, nullptr, -1, 1, 0, 0, *data.bits);
  }
  const int rc = (g_active != nullptr && g_active->write_mem != nullptr)
                     ? g_active->write_mem(g_active->mem_ctx, addr, buf, len, kind)
                     : -1;
  return rc == 0;
}

} // namespace

// ---- the memory seam (strong overrides of the weakified runtime) ----

extern "C" {

void emulator_read_mem(lbits *data, const uint64_t addr_size, const sbits addr, const mpz_t n) {
  (void)addr_size;
  seam_read(data, addr.bits, mpz_get_ui(n), ORACLE_MEM_READ);
}

void emulator_read_mem_ifetch(lbits *data, const uint64_t addr_size, const sbits addr, const mpz_t n) {
  (void)addr_size;
  seam_read(data, addr.bits, mpz_get_ui(n), ORACLE_MEM_IFETCH);
}

void emulator_read_mem_exclusive(lbits *data, const uint64_t addr_size, const sbits addr, const mpz_t n) {
  (void)addr_size;
  seam_read(data, addr.bits, mpz_get_ui(n), ORACLE_MEM_READ_EXCL);
}

bool emulator_write_mem(const uint64_t addr_size, const sbits addr, const mpz_t n, const lbits data) {
  (void)addr_size;
  return seam_write(addr.bits, mpz_get_ui(n), data, ORACLE_MEM_WRITE);
}

bool emulator_write_mem_exclusive(const uint64_t addr_size, const sbits addr, const mpz_t n, const lbits data) {
  (void)addr_size;
  return seam_write(addr.bits, mpz_get_ui(n), data, ORACLE_MEM_WRITE_EXCL);
}

} // extern "C"

// ---- C ABI ----

extern "C" {

oracle *oracle_create(const char *config_json_path, uint64_t reset_pc) {
  if (g_active != nullptr) {
    fprintf(stderr, "oracle: only one oracle may exist at a time (the Sail memory seam is process-global)\n");
    return nullptr;
  }
  auto *o = new OracleImpl();
  g_active = o;
  if (!o->init(config_json_path, reset_pc)) {
    g_active = nullptr;
    delete o;
    return nullptr;
  }
  return reinterpret_cast<oracle *>(o);
}

void oracle_destroy(oracle *o) {
  auto *impl = reinterpret_cast<OracleImpl *>(o);
  if (impl == nullptr || impl != g_active) {
    return;
  }
  g_active = nullptr;
  delete impl;
}

void oracle_set_mem_callbacks(oracle *o, oracle_read_mem_fn read_mem, oracle_write_mem_fn write_mem, void *ctx) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  impl.read_mem = read_mem;
  impl.write_mem = write_mem;
  impl.mem_ctx = ctx;
}

void oracle_set_observers(oracle *o, oracle_trap_fn trap, oracle_term_fn term, void *ctx) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  impl.trap = trap;
  impl.term = term;
  impl.obs_ctx = ctx;
}

int oracle_step(oracle *o) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  static uint64_t step_no = 0;
  const bool waiting = impl.step(step_no++);
  if (impl.trapped()) {
    return ORACLE_TRAP;
  }
  return waiting ? ORACLE_WAITING : ORACLE_OK;
}

int oracle_trap_cause(oracle *o, uint64_t *cause) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  if (!impl.trapped() || cause == nullptr) {
    return ORACLE_ERR;
  }
  int is_interrupt = 0;
  uint64_t c = 0;
  impl.trap_info(&is_interrupt, &c);
  const uint64_t mask = (UINT64_C(1) << 63) - 1;
  *cause = (static_cast<uint64_t>(is_interrupt) << 63) | (c & mask);
  return ORACLE_OK;
}

void oracle_tick_clock(oracle *o) { reinterpret_cast<OracleImpl *>(o)->tick_clock(); }

// The generated model's accessors are not const, so the const C-ABI
// signatures cast at this boundary.
uint64_t oracle_xlen(const oracle *o) {
  return const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->xlen();
}

int oracle_get_pc(const oracle *o, uint64_t *pc) {
  if (pc == nullptr) {
    return ORACLE_ERR;
  }
  *pc = const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->pc();
  return ORACLE_OK;
}

int oracle_set_pc(oracle *o, uint64_t pc) {
  reinterpret_cast<OracleImpl *>(o)->set_pc(pc);
  return ORACLE_OK;
}

int oracle_get_x(const oracle *o, unsigned reg, uint64_t *val) {
  if (reg > 31 || val == nullptr) {
    return ORACLE_ERR;
  }
  *val = const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->xreg(reg);
  return ORACLE_OK;
}

int oracle_set_x(oracle *o, unsigned reg, uint64_t val) {
  if (reg > 31) {
    return ORACLE_ERR;
  }
  if (reg != 0) {
    reinterpret_cast<OracleImpl *>(o)->set_xreg(reg, val);
  }
  return ORACLE_OK;
}

int oracle_get_f(const oracle *o, unsigned reg, uint64_t *bits) {
  if (reg > 31 || bits == nullptr) {
    return ORACLE_ERR;
  }
  *bits = const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->freg(reg);
  return ORACLE_OK;
}

int oracle_set_f(oracle *o, unsigned reg, uint64_t bits) {
  if (reg > 31) {
    return ORACLE_ERR;
  }
  reinterpret_cast<OracleImpl *>(o)->set_freg(reg, bits);
  return ORACLE_OK;
}

int oracle_get_csr(oracle *o, uint16_t csr_addr, uint64_t *val) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  if (val == nullptr || !impl.csr_readable(csr_addr)) {
    return ORACLE_ERR;
  }
  *val = impl.get_csr(csr_addr);
  return ORACLE_OK;
}

int oracle_set_csr(oracle *o, uint16_t csr_addr, uint64_t val) {
  auto &impl = *reinterpret_cast<OracleImpl *>(o);
  return impl.set_csr(csr_addr, val) ? ORACLE_OK : ORACLE_ERR;
}

int oracle_csr_count(const oracle *o) {
  return static_cast<int>(const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->csr_list().size());
}

int oracle_csr_at(const oracle *o, unsigned index, uint16_t *csr_addr) {
  const auto &csrs = const_cast<OracleImpl *>(reinterpret_cast<const OracleImpl *>(o))->csr_list();
  if (index >= csrs.size() || csr_addr == nullptr) {
    return ORACLE_ERR;
  }
  *csr_addr = csrs[index];
  return ORACLE_OK;
}

const char *oracle_sail_version(void) {
  return "sail " SAIL_ORACLE_SAIL_VERSION ", sail-riscv " SAIL_ORACLE_MODEL_COMMIT;
}

const char *oracle_isa_string(oracle *o) { return reinterpret_cast<OracleImpl *>(o)->isa_string(); }

} // extern "C"
