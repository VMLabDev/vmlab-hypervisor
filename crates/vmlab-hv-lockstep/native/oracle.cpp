// SPDX-License-Identifier: MIT
// Handwritten adapter. The linked generated model retains its upstream licence.
#include "sail_riscv_model.h"
#include <jsoncons/json.hpp>
#include <algorithm>
#include <fstream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

using Read = bool (*)(void *, uint64_t, uint8_t *, size_t);
using Write = bool (*)(void *, uint64_t, const uint8_t *, size_t);

struct Oracle {
    hart::Model model;
    void *memory;
    Read read;
    Write write;
    std::string error;
    int64_t steps = 0;
    bool initialized = false;
    ~Oracle() { if (initialized) model.model_fini(); }
};

static thread_local Oracle *active = nullptr;
static thread_local std::string last_error;

extern "C" void __wrap_emulator_read_mem(lbits *result, uint64_t, sbits address, const mpz_t count) {
    const size_t n = mpz_get_ui(count);
    if (n > 4096) throw std::runtime_error("oracle memory read exceeds 4096 bytes");
    std::vector<uint8_t> bytes(n);
    if (!active || !active->read(active->memory, address.bits, bytes.data(), n))
        throw std::runtime_error("oracle read outside guest memory");
    result->len = n * 8;
    mpz_import(*result->bits, n, -1, 1, 0, 0, bytes.data());
}
extern "C" void __wrap_emulator_read_mem_ifetch(lbits *r, uint64_t w, sbits a, const mpz_t n) {
    __wrap_emulator_read_mem(r, w, a, n);
}
extern "C" void __wrap_emulator_read_mem_exclusive(lbits *r, uint64_t w, sbits a, const mpz_t n) {
    __wrap_emulator_read_mem(r, w, a, n);
}
extern "C" bool __wrap_emulator_write_mem(uint64_t, sbits address, const mpz_t count, lbits data) {
    const size_t n = mpz_get_ui(count);
    if (n > 4096 || mpz_sizeinbase(*data.bits, 2) > n * 8)
        throw std::runtime_error("invalid oracle memory write width");
    std::vector<uint8_t> bytes(n);
    size_t written = 0;
    mpz_export(bytes.data(), &written, -1, 1, 0, 0, *data.bits);
    if (!active || !active->write(active->memory, address.bits, bytes.data(), n))
        throw std::runtime_error("oracle write outside guest memory");
    return true;
}
extern "C" bool __wrap_emulator_write_mem_exclusive(uint64_t w, sbits a, const mpz_t n, lbits d) {
    return __wrap_emulator_write_mem(w, a, n, d);
}

extern "C" const char *vmlab_oracle_error() { return last_error.c_str(); }

extern "C" void *vmlab_oracle_new(const char *config, uint64_t pc, void *memory, Read read, Write write) {
    try {
        std::ifstream input(config);
        if (!input) throw std::runtime_error("cannot open Sail config");
        auto json = jsoncons::json::parse(input);
        auto text = json.to_string();
        sail_config_set_string(text.c_str());
        auto oracle = std::make_unique<Oracle>();
        oracle->memory = memory; oracle->read = read; oracle->write = write;
        active = oracle.get();
        oracle->model.model_init();
        oracle->initialized = true;
        if (oracle->model.zxlen != 64) throw std::runtime_error("lockstep requires RV64");
        oracle->model.zset_pc_reset_address(pc);
        oracle->model.zinit_model(config);
        oracle->model.zinit_boot_requirements(UNIT);
        active = nullptr;
        return oracle.release();
    } catch (const std::exception &e) { last_error = e.what(); active = nullptr; return nullptr; }
}

extern "C" void vmlab_oracle_free(Oracle *o) { delete o; }

struct State {
    uint64_t gpr[32], fpr[32], pc;
    uint8_t privilege, virtualized;
};

extern "C" int vmlab_oracle_state(Oracle *o, State *s, bool set) {
    try {
        auto &m = o->model;
        if (set) {
            const auto saved_mstatus = m.zmstatus;
            const auto saved_vsstatus = m.zvsstatus;
            for (int i = 0; i < 32; ++i) {
                m.zwX(i, sbits{64, s->gpr[i]});
                m.zwF(i, sbits{64, s->fpr[i]});
            }
            // wF marks FS dirty. Snapshot restore must preserve supplied status.
            m.zmstatus = saved_mstatus;
            m.zvsstatus = saved_vsstatus;
            m.zPC = sbits{64, s->pc};
            m.znextPC = m.zPC;
            if (s->privilege == 3 && !s->virtualized) m.zcur_privilege = hart::zMachine;
            else if (s->privilege == 1) m.zcur_privilege = s->virtualized ? hart::zVirtualSupervisor : hart::zSupervisor;
            else if (s->privilege == 0) m.zcur_privilege = s->virtualized ? hart::zVirtualUser : hart::zUser;
            else throw std::runtime_error("invalid privilege");
        } else {
            for (int i = 0; i < 32; ++i) { s->gpr[i] = m.zrX(i).bits; s->fpr[i] = m.zrF(i).bits; }
            s->pc = m.zPC.bits;
            s->privilege = m.zcur_privilege == hart::zMachine ? 3 :
                (m.zcur_privilege == hart::zSupervisor || m.zcur_privilege == hart::zVirtualSupervisor ? 1 : 0);
            s->virtualized = m.zcur_privilege == hart::zVirtualUser || m.zcur_privilege == hart::zVirtualSupervisor;
        }
        return 0;
    } catch (const std::exception &e) { last_error = e.what(); return -1; }
}

extern "C" int vmlab_oracle_csr(Oracle *o, uint16_t id, uint64_t *value, bool set) {
    try {
        // Snapshot access bypasses dynamic privilege/FS checks. Reject unknown
        // IDs before calling the generated read_CSR fallback, which terminates.
        static const std::vector<uint16_t> ids = {
            0x001,0x002,0x003,0x100,0x104,0x105,0x106,0x140,0x141,0x142,
            0x143,0x144,0x180,0x300,0x301,0x302,0x303,0x304,0x305,0x306,
            0x340,0x341,0x342,0x343,0x344,0xb00,0xb02
        };
        if (std::find(ids.begin(), ids.end(), id) == ids.end())
            throw std::runtime_error("CSR is outside the adapter's RV64GC comparison set");
        if (set) {
            hart::zresultzIbzCuzK result;
            o->model.zwrite_CSR(&result, id, sbits{64, *value});
        } else *value = o->model.zread_CSR(id).bits;
        return 0;
    } catch (const std::exception &e) { last_error = e.what(); return -1; }
}

extern "C" int vmlab_oracle_step(Oracle *o) {
    try {
        active = o;
        sail_int count;
        CREATE(sail_int)(&count);
        CONVERT_OF(sail_int, mach_int)(&count, o->steps++);
        o->model.ztry_step(count, false);
        KILL(sail_int)(&count);
        o->model.ztick_clock(UNIT);
        active = nullptr;
        if (o->model.have_exception) throw std::runtime_error("Sail model internal exception");
        return 0;
    } catch (const std::exception &e) { last_error = e.what(); active = nullptr; return -1; }
}
