// RISCV Sail Model
//
// This Sail RISC-V architecture model, comprising all files and
// directories except for the third party dependencies in the
// 'dependencies' directory (which include copies of their licences), is
// subject to the BSD two-clause licence below.
//
// Copyright (c) 2017-2025
//    ahadali5000
//    Alasdair Armstrong
//    Alexander Richardson
//    Aril Computer Corp., for contributions by Scott Johnson
//    Ben Marshall
//    Bicheng Yang
//    Bilal Sakhawat
//    Brian Campbell
//    Chris Casinghino
//    Christopher Pulte
//    Codasip, for contributions by Tim Hutt, Martin Berger and Ben Fletcher
//    dylux
//    eroom1966
//    Google LLC, for contributions by its employees
//    Hesham Almatary
//    Jan Henrik Weinstock
//    Jessica Clarke
//    Jon French
//    Jordan Carlin
//    Martin Berger
//    Michael Sammler
//    Microsoft, for contributions by Robert Norton-Wright and Nathaniel Wesley Filardo
//    Muhammad Bilal Sakhawat
//    Nathaniel Wesley Filardo
//    Paul A. Clarke
//    Peter Rugg
//    Peter Sewell
//    Philipp Tomsich
//    Prashanth Mundkur
//    Rafael Sene
//    Rishiyur S. Nikhil (Bluespec, Inc.)
//    Robert Norton-Wright
//    Scott Johnson (Aril Inc.)
//    Shaked Flur
//    Thibaut Pérami
//    Thomas Bauereiss
//    VRULL GmbH, for contributions by its employees
//    William McSpaddden
//    Xinlai Wan
//
// For a complete list of authors run this command:
//
//     git shortlog --summary --numbered --email
//
// All rights reserved.
//
// This software was developed by the above within the Rigorous
// Engineering of Mainstream Systems (REMS) project, partly funded by
// EPSRC grant EP/K008528/1, at the Universities of Cambridge and
// Edinburgh.
//
// This software was developed by SRI International and the University of
// Cambridge Computer Laboratory (Department of Computer Science and
// Technology) under DARPA/AFRL contract FA8650-18-C-7809 ("CIFV"), and
// under DARPA contract HR0011-18-C-0016 ("ECATS") as part of the DARPA
// SSITH research programme.
//
// This project has received funding from the European Research Council
// (ERC) under the European Union’s Horizon 2020 research and innovation
// programme (grant agreement 789108, ELVER).
//
// ------------------------------------------------------------------
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS''
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
// TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
// PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF
// USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
// OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.
// Sail and the Sail architecture models here, comprising all files and
// directories except the ASL-derived Sail code in the aarch64 directory,
// the logo in the etc/logo/ directory (by Jean Pichon, under CC0), and
// various exceptions detailed in THIRD_PARTY_FILES.md are subject to the
// BSD two-clause licence below.
//
// The ASL derived parts of the ARMv8.3 specification in
// aarch64/no_vector and aarch64/full are copyright ARM Ltd.
//
// This work was partially supported by EPSRC grant EP/K008528/1
// REMS: Rigorous Engineering for Mainstream Systems, an ARM iCASE award,
// EPSRC IAA KTF funding, and donations from Arm. This project has
// received funding from the European Research Council (ERC) under the
// European Union’s Horizon 2020 research and innovation programme (grant
// agreement No 789108, ELVER).
//
// This software was developed by SRI International and the University of
// Cambridge Computer Laboratory (Department of Computer Science and
// Technology) under DARPA/AFRL contracts FA8650-18-C-7809 ("CIFV")
// and FA8750-10-C-0237 ("CTSRD").
//
// -----
//
// Sail
//
// Copyright (c) 2013-2025
//   Kathyrn Gray
//   Shaked Flur
//   Stephen Kell
//   Gabriel Kerneis
//   Robert Norton-Wright
//   Christopher Pulte
//   Peter Sewell
//   Alasdair Armstrong
//   Brian Campbell
//   Thomas Bauereiss
//   Anthony Fox
//   Jon French
//   Dominic Mulligan
//   Stephen Kell
//   Mark Wassell
//   Alastair Reid (Arm Ltd)
//
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS''
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
// TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
// PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF
// USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
// OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.
// Generated by vmlab-hv-tcg-emit. Do not edit.
// Sail RISC-V model, BSD-2-Clause. See ../LICENSE.sail-riscv.
// Original copyrights and terms are retained in that file and THIRD_PARTY_NOTICES.
pub const IR_SHA256: &str = "78c0fe0b8aef56fa7f41471331cb076254cba8512c20b2bb5d0f446832be7aaa";
pub const ENCODED_IR_BYTES: usize = 21876036;
use std::io::Read;
use std::sync::OnceLock;
use vmlab_hv_tcg_ir::{Function, Program};
pub fn model() -> &'static Program {
    static MODEL: OnceLock<Program> = OnceLock::new();
    MODEL.get_or_init(|| {
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(include_bytes!("model.json.gz").as_slice())
            .read_to_end(&mut json)
            .expect("generated model gzip");
        serde_json::from_slice(&json).expect("generated model IR")
    })
}
pub fn decoder(compressed: bool) -> &'static Function {
    &model().functions[if compressed {
        "zencdec_compressed_backwards"
    } else {
        "zencdec_backwards"
    }]
}
pub fn build_zADDIW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zADDIW");
    function
}
pub fn build_zAES32DSI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES32DSI");
    function
}
pub fn build_zAES32DSMI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES32DSMI");
    function
}
pub fn build_zAES32ESI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES32ESI");
    function
}
pub fn build_zAES32ESMI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES32ESMI");
    function
}
pub fn build_zAES64DS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64DS");
    function
}
pub fn build_zAES64DSM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64DSM");
    function
}
pub fn build_zAES64ES() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64ES");
    function
}
pub fn build_zAES64ESM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64ESM");
    function
}
pub fn build_zAES64IM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64IM");
    function
}
pub fn build_zAES64KS1I() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64KS1I");
    function
}
pub fn build_zAES64KS2() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAES64KS2");
    function
}
pub fn build_zAMO() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zAMO");
    function
}
pub fn build_zBITYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zBITYPE");
    function
}
pub fn build_zBREV8() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zBREV8");
    function
}
pub fn build_zBTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zBTYPE");
    function
}
pub fn build_zCLMUL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCLMUL");
    function
}
pub fn build_zCLMULH() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCLMULH");
    function
}
pub fn build_zCLMULR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCLMULR");
    function
}
pub fn build_zCLZ() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCLZ");
    function
}
pub fn build_zCLZW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCLZW");
    function
}
pub fn build_zCPOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCPOP");
    function
}
pub fn build_zCPOPW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCPOPW");
    function
}
pub fn build_zCSRImm() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCSRImm");
    function
}
pub fn build_zCSRReg() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCSRReg");
    function
}
pub fn build_zCTZ() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCTZ");
    function
}
pub fn build_zCTZW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zCTZW");
    function
}
pub fn build_zC_ADD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADD");
    function
}
pub fn build_zC_ADDI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADDI");
    function
}
pub fn build_zC_ADDI16SP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADDI16SP");
    function
}
pub fn build_zC_ADDI4SPN() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADDI4SPN");
    function
}
pub fn build_zC_ADDIW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADDIW");
    function
}
pub fn build_zC_ADDW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ADDW");
    function
}
pub fn build_zC_AND() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_AND");
    function
}
pub fn build_zC_ANDI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ANDI");
    function
}
pub fn build_zC_BEQZ() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_BEQZ");
    function
}
pub fn build_zC_BNEZ() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_BNEZ");
    function
}
pub fn build_zC_EBREAK() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_EBREAK");
    function
}
pub fn build_zC_FLD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FLD");
    function
}
pub fn build_zC_FLDSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FLDSP");
    function
}
pub fn build_zC_FLW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FLW");
    function
}
pub fn build_zC_FLWSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FLWSP");
    function
}
pub fn build_zC_FSD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FSD");
    function
}
pub fn build_zC_FSDSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FSDSP");
    function
}
pub fn build_zC_FSW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FSW");
    function
}
pub fn build_zC_FSWSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_FSWSP");
    function
}
pub fn build_zC_ILLEGAL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ILLEGAL");
    function
}
pub fn build_zC_J() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_J");
    function
}
pub fn build_zC_JAL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_JAL");
    function
}
pub fn build_zC_JALR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_JALR");
    function
}
pub fn build_zC_JR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_JR");
    function
}
pub fn build_zC_LBU() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LBU");
    function
}
pub fn build_zC_LD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LD");
    function
}
pub fn build_zC_LDSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LDSP");
    function
}
pub fn build_zC_LH() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LH");
    function
}
pub fn build_zC_LHU() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LHU");
    function
}
pub fn build_zC_LI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LI");
    function
}
pub fn build_zC_LUI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LUI");
    function
}
pub fn build_zC_LW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LW");
    function
}
pub fn build_zC_LWSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_LWSP");
    function
}
pub fn build_zC_MUL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_MUL");
    function
}
pub fn build_zC_MV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_MV");
    function
}
pub fn build_zC_NOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_NOP");
    function
}
pub fn build_zC_NOT() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_NOT");
    function
}
pub fn build_zC_NTL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_NTL");
    function
}
pub fn build_zC_OR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_OR");
    function
}
pub fn build_zC_SB() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SB");
    function
}
pub fn build_zC_SD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SD");
    function
}
pub fn build_zC_SDSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SDSP");
    function
}
pub fn build_zC_SEXT_B() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SEXT_B");
    function
}
pub fn build_zC_SEXT_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SEXT_H");
    function
}
pub fn build_zC_SH() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SH");
    function
}
pub fn build_zC_SLLI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SLLI");
    function
}
pub fn build_zC_SRAI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SRAI");
    function
}
pub fn build_zC_SRLI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SRLI");
    function
}
pub fn build_zC_SSPOPCHK() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SSPOPCHK");
    function
}
pub fn build_zC_SSPUSH() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SSPUSH");
    function
}
pub fn build_zC_SUB() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SUB");
    function
}
pub fn build_zC_SUBW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SUBW");
    function
}
pub fn build_zC_SW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SW");
    function
}
pub fn build_zC_SWSP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_SWSP");
    function
}
pub fn build_zC_XOR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_XOR");
    function
}
pub fn build_zC_ZEXT_B() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ZEXT_B");
    function
}
pub fn build_zC_ZEXT_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ZEXT_H");
    function
}
pub fn build_zC_ZEXT_W() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zC_ZEXT_W");
    function
}
pub fn build_zDIV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zDIV");
    function
}
pub fn build_zDIVW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zDIVW");
    function
}
pub fn build_zEBREAK() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zEBREAK");
    function
}
pub fn build_zECALL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zECALL");
    function
}
pub fn build_zFCVTMOD_W_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFCVTMOD_W_D");
    function
}
pub fn build_zFCVT_BF16_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFCVT_BF16_S");
    function
}
pub fn build_zFCVT_S_BF16() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFCVT_S_BF16");
    function
}
pub fn build_zFENCE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFENCE");
    function
}
pub fn build_zFENCEI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFENCEI");
    function
}
pub fn build_zFENCE_TSO() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFENCE_TSO");
    function
}
pub fn build_zFLEQ_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLEQ_D");
    function
}
pub fn build_zFLEQ_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLEQ_H");
    function
}
pub fn build_zFLEQ_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLEQ_S");
    function
}
pub fn build_zFLI_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLI_D");
    function
}
pub fn build_zFLI_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLI_H");
    function
}
pub fn build_zFLI_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLI_S");
    function
}
pub fn build_zFLTQ_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLTQ_D");
    function
}
pub fn build_zFLTQ_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLTQ_H");
    function
}
pub fn build_zFLTQ_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFLTQ_S");
    function
}
pub fn build_zFMAXM_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMAXM_D");
    function
}
pub fn build_zFMAXM_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMAXM_H");
    function
}
pub fn build_zFMAXM_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMAXM_S");
    function
}
pub fn build_zFMINM_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMINM_D");
    function
}
pub fn build_zFMINM_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMINM_H");
    function
}
pub fn build_zFMINM_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMINM_S");
    function
}
pub fn build_zFMVH_X_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMVH_X_D");
    function
}
pub fn build_zFMVP_D_X() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFMVP_D_X");
    function
}
pub fn build_zFROUNDNX_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUNDNX_D");
    function
}
pub fn build_zFROUNDNX_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUNDNX_H");
    function
}
pub fn build_zFROUNDNX_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUNDNX_S");
    function
}
pub fn build_zFROUND_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUND_D");
    function
}
pub fn build_zFROUND_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUND_H");
    function
}
pub fn build_zFROUND_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFROUND_S");
    function
}
pub fn build_zFVFMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVFMATYPE");
    function
}
pub fn build_zFVFMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVFMTYPE");
    function
}
pub fn build_zFVFTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVFTYPE");
    function
}
pub fn build_zFVVMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVVMATYPE");
    function
}
pub fn build_zFVVMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVVMTYPE");
    function
}
pub fn build_zFVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFVVTYPE");
    function
}
pub fn build_zFWFTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWFTYPE");
    function
}
pub fn build_zFWVFMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWVFMATYPE");
    function
}
pub fn build_zFWVFTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWVFTYPE");
    function
}
pub fn build_zFWVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWVTYPE");
    function
}
pub fn build_zFWVVMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWVVMATYPE");
    function
}
pub fn build_zFWVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zFWVVTYPE");
    function
}
pub fn build_zF_BIN_F_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_F_TYPE_D");
    function
}
pub fn build_zF_BIN_F_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_F_TYPE_H");
    function
}
pub fn build_zF_BIN_RM_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_RM_TYPE_D");
    function
}
pub fn build_zF_BIN_RM_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_RM_TYPE_H");
    function
}
pub fn build_zF_BIN_RM_TYPE_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_RM_TYPE_S");
    function
}
pub fn build_zF_BIN_TYPE_F_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_TYPE_F_S");
    function
}
pub fn build_zF_BIN_TYPE_X_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_TYPE_X_S");
    function
}
pub fn build_zF_BIN_X_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_X_TYPE_D");
    function
}
pub fn build_zF_BIN_X_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_BIN_X_TYPE_H");
    function
}
pub fn build_zF_MADD_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_MADD_TYPE_D");
    function
}
pub fn build_zF_MADD_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_MADD_TYPE_H");
    function
}
pub fn build_zF_MADD_TYPE_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_MADD_TYPE_S");
    function
}
pub fn build_zF_UN_F_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_F_TYPE_D");
    function
}
pub fn build_zF_UN_F_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_F_TYPE_H");
    function
}
pub fn build_zF_UN_RM_FF_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FF_TYPE_D");
    function
}
pub fn build_zF_UN_RM_FF_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FF_TYPE_H");
    function
}
pub fn build_zF_UN_RM_FF_TYPE_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FF_TYPE_S");
    function
}
pub fn build_zF_UN_RM_FX_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FX_TYPE_D");
    function
}
pub fn build_zF_UN_RM_FX_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FX_TYPE_H");
    function
}
pub fn build_zF_UN_RM_FX_TYPE_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_FX_TYPE_S");
    function
}
pub fn build_zF_UN_RM_XF_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_XF_TYPE_D");
    function
}
pub fn build_zF_UN_RM_XF_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_XF_TYPE_H");
    function
}
pub fn build_zF_UN_RM_XF_TYPE_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_RM_XF_TYPE_S");
    function
}
pub fn build_zF_UN_TYPE_F_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_TYPE_F_S");
    function
}
pub fn build_zF_UN_TYPE_X_S() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_TYPE_X_S");
    function
}
pub fn build_zF_UN_X_TYPE_D() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_X_TYPE_D");
    function
}
pub fn build_zF_UN_X_TYPE_H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zF_UN_X_TYPE_H");
    function
}
pub fn build_zHFENCE_GVMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHFENCE_GVMA");
    function
}
pub fn build_zHFENCE_VVMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHFENCE_VVMA");
    function
}
pub fn build_zHINVAL_GVMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHINVAL_GVMA");
    function
}
pub fn build_zHINVAL_VVMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHINVAL_VVMA");
    function
}
pub fn build_zHLVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHLVTYPE");
    function
}
pub fn build_zHSV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zHSV");
    function
}
pub fn build_zILLEGAL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zILLEGAL");
    function
}
pub fn build_zITYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zITYPE");
    function
}
pub fn build_zJAL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zJAL");
    function
}
pub fn build_zJALR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zJALR");
    function
}
pub fn build_zLOAD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zLOAD");
    function
}
pub fn build_zLOADRES() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zLOADRES");
    function
}
pub fn build_zLOAD_FP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zLOAD_FP");
    function
}
pub fn build_zLPAD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zLPAD");
    function
}
pub fn build_zMASKTYPEI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMASKTYPEI");
    function
}
pub fn build_zMASKTYPEV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMASKTYPEV");
    function
}
pub fn build_zMASKTYPEX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMASKTYPEX");
    function
}
pub fn build_zMMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMMTYPE");
    function
}
pub fn build_zMOVETYPEI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMOVETYPEI");
    function
}
pub fn build_zMOVETYPEV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMOVETYPEV");
    function
}
pub fn build_zMOVETYPEX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMOVETYPEX");
    function
}
pub fn build_zMRET() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMRET");
    function
}
pub fn build_zMUL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMUL");
    function
}
pub fn build_zMULW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMULW");
    function
}
pub fn build_zMVVCOMPRESS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMVVCOMPRESS");
    function
}
pub fn build_zMVVMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMVVMATYPE");
    function
}
pub fn build_zMVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMVVTYPE");
    function
}
pub fn build_zMVXMATYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMVXMATYPE");
    function
}
pub fn build_zMVXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zMVXTYPE");
    function
}
pub fn build_zNISTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNISTYPE");
    function
}
pub fn build_zNITYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNITYPE");
    function
}
pub fn build_zNTL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNTL");
    function
}
pub fn build_zNVSTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNVSTYPE");
    function
}
pub fn build_zNVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNVTYPE");
    function
}
pub fn build_zNXSTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNXSTYPE");
    function
}
pub fn build_zNXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zNXTYPE");
    function
}
pub fn build_zORCB() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zORCB");
    function
}
pub fn build_zPAUSE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zPAUSE");
    function
}
pub fn build_zREM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zREM");
    function
}
pub fn build_zREMW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zREMW");
    function
}
pub fn build_zREV8() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zREV8");
    function
}
pub fn build_zRFVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRFVVTYPE");
    function
}
pub fn build_zRFWVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRFWVVTYPE");
    function
}
pub fn build_zRIVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRIVVTYPE");
    function
}
pub fn build_zRMVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRMVVTYPE");
    function
}
pub fn build_zRORI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRORI");
    function
}
pub fn build_zRORIW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRORIW");
    function
}
pub fn build_zRTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRTYPE");
    function
}
pub fn build_zRTYPEW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zRTYPEW");
    function
}
pub fn build_zSFENCE_INVAL_IR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSFENCE_INVAL_IR");
    function
}
pub fn build_zSFENCE_VMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSFENCE_VMA");
    function
}
pub fn build_zSFENCE_W_INVAL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSFENCE_W_INVAL");
    function
}
pub fn build_zSHA256SIG0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA256SIG0");
    function
}
pub fn build_zSHA256SIG1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA256SIG1");
    function
}
pub fn build_zSHA256SUM0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA256SUM0");
    function
}
pub fn build_zSHA256SUM1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA256SUM1");
    function
}
pub fn build_zSHA512SIG0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG0");
    function
}
pub fn build_zSHA512SIG0H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG0H");
    function
}
pub fn build_zSHA512SIG0L() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG0L");
    function
}
pub fn build_zSHA512SIG1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG1");
    function
}
pub fn build_zSHA512SIG1H() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG1H");
    function
}
pub fn build_zSHA512SIG1L() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SIG1L");
    function
}
pub fn build_zSHA512SUM0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SUM0");
    function
}
pub fn build_zSHA512SUM0R() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SUM0R");
    function
}
pub fn build_zSHA512SUM1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SUM1");
    function
}
pub fn build_zSHA512SUM1R() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHA512SUM1R");
    function
}
pub fn build_zSHIFTIOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHIFTIOP");
    function
}
pub fn build_zSHIFTIWOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSHIFTIWOP");
    function
}
pub fn build_zSINVAL_VMA() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSINVAL_VMA");
    function
}
pub fn build_zSLLIUW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSLLIUW");
    function
}
pub fn build_zSM3P0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSM3P0");
    function
}
pub fn build_zSM3P1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSM3P1");
    function
}
pub fn build_zSM4ED() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSM4ED");
    function
}
pub fn build_zSM4KS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSM4KS");
    function
}
pub fn build_zSRET() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSRET");
    function
}
pub fn build_zSSAMOSWAP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSSAMOSWAP");
    function
}
pub fn build_zSSPOPCHK() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSSPOPCHK");
    function
}
pub fn build_zSSPUSH() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSSPUSH");
    function
}
pub fn build_zSSRDP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSSRDP");
    function
}
pub fn build_zSTORE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSTORE");
    function
}
pub fn build_zSTORECON() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSTORECON");
    function
}
pub fn build_zSTORE_FP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zSTORE_FP");
    function
}
pub fn build_zUNZIP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zUNZIP");
    function
}
pub fn build_zUTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zUTYPE");
    function
}
pub fn build_zVAESDF() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESDF");
    function
}
pub fn build_zVAESDM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESDM");
    function
}
pub fn build_zVAESEF() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESEF");
    function
}
pub fn build_zVAESEM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESEM");
    function
}
pub fn build_zVAESKF1_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESKF1_VI");
    function
}
pub fn build_zVAESKF2_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESKF2_VI");
    function
}
pub fn build_zVAESZ_VS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVAESZ_VS");
    function
}
pub fn build_zVANDN_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVANDN_VV");
    function
}
pub fn build_zVANDN_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVANDN_VX");
    function
}
pub fn build_zVBREV8_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVBREV8_V");
    function
}
pub fn build_zVBREV_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVBREV_V");
    function
}
pub fn build_zVCLMULH_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCLMULH_VV");
    function
}
pub fn build_zVCLMULH_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCLMULH_VX");
    function
}
pub fn build_zVCLMUL_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCLMUL_VV");
    function
}
pub fn build_zVCLMUL_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCLMUL_VX");
    function
}
pub fn build_zVCLZ_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCLZ_V");
    function
}
pub fn build_zVCPOP_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCPOP_M");
    function
}
pub fn build_zVCPOP_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCPOP_V");
    function
}
pub fn build_zVCTZ_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVCTZ_V");
    function
}
pub fn build_zVEXTTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVEXTTYPE");
    function
}
pub fn build_zVFIRST_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFIRST_M");
    function
}
pub fn build_zVFMERGE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFMERGE");
    function
}
pub fn build_zVFMV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFMV");
    function
}
pub fn build_zVFMVFS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFMVFS");
    function
}
pub fn build_zVFMVSF() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFMVSF");
    function
}
pub fn build_zVFNCVTBF16_F_F_W() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFNCVTBF16_F_F_W");
    function
}
pub fn build_zVFNUNARY0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFNUNARY0");
    function
}
pub fn build_zVFUNARY0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFUNARY0");
    function
}
pub fn build_zVFUNARY1() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFUNARY1");
    function
}
pub fn build_zVFWCVTBF16_F_F_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFWCVTBF16_F_F_V");
    function
}
pub fn build_zVFWMACCBF16_VF() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFWMACCBF16_VF");
    function
}
pub fn build_zVFWMACCBF16_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFWMACCBF16_VV");
    function
}
pub fn build_zVFWUNARY0() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVFWUNARY0");
    function
}
pub fn build_zVGHSH_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVGHSH_VV");
    function
}
pub fn build_zVGMUL_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVGMUL_VV");
    function
}
pub fn build_zVICMPTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVICMPTYPE");
    function
}
pub fn build_zVID_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVID_V");
    function
}
pub fn build_zVIMCTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVIMCTYPE");
    function
}
pub fn build_zVIMSTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVIMSTYPE");
    function
}
pub fn build_zVIMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVIMTYPE");
    function
}
pub fn build_zVIOTA_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVIOTA_M");
    function
}
pub fn build_zVISG() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVISG");
    function
}
pub fn build_zVITYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVITYPE");
    function
}
pub fn build_zVLRETYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVLRETYPE");
    function
}
pub fn build_zVLSEGFFTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVLSEGFFTYPE");
    function
}
pub fn build_zVLSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVLSEGTYPE");
    function
}
pub fn build_zVLSSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVLSSEGTYPE");
    function
}
pub fn build_zVLXSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVLXSEGTYPE");
    function
}
pub fn build_zVMSBF_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMSBF_M");
    function
}
pub fn build_zVMSIF_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMSIF_M");
    function
}
pub fn build_zVMSOF_M() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMSOF_M");
    function
}
pub fn build_zVMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMTYPE");
    function
}
pub fn build_zVMVRTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMVRTYPE");
    function
}
pub fn build_zVMVSX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMVSX");
    function
}
pub fn build_zVMVXS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVMVXS");
    function
}
pub fn build_zVREV8_V() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVREV8_V");
    function
}
pub fn build_zVROL_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVROL_VV");
    function
}
pub fn build_zVROL_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVROL_VX");
    function
}
pub fn build_zVROR_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVROR_VI");
    function
}
pub fn build_zVROR_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVROR_VV");
    function
}
pub fn build_zVROR_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVROR_VX");
    function
}
pub fn build_zVSETIVLI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSETIVLI");
    function
}
pub fn build_zVSETVL() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSETVL");
    function
}
pub fn build_zVSETVLI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSETVLI");
    function
}
pub fn build_zVSHA2MS_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSHA2MS_VV");
    function
}
pub fn build_zVSM3C_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSM3C_VI");
    function
}
pub fn build_zVSM3ME_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSM3ME_VV");
    function
}
pub fn build_zVSM4K_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSM4K_VI");
    function
}
pub fn build_zVSRETYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSRETYPE");
    function
}
pub fn build_zVSSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSSEGTYPE");
    function
}
pub fn build_zVSSSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSSSEGTYPE");
    function
}
pub fn build_zVSXSEGTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVSXSEGTYPE");
    function
}
pub fn build_zVVCMPTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVVCMPTYPE");
    function
}
pub fn build_zVVMCTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVVMCTYPE");
    function
}
pub fn build_zVVMSTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVVMSTYPE");
    function
}
pub fn build_zVVMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVVMTYPE");
    function
}
pub fn build_zVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVVTYPE");
    function
}
pub fn build_zVWSLL_VI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVWSLL_VI");
    function
}
pub fn build_zVWSLL_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVWSLL_VV");
    function
}
pub fn build_zVWSLL_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVWSLL_VX");
    function
}
pub fn build_zVXCMPTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXCMPTYPE");
    function
}
pub fn build_zVXMCTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXMCTYPE");
    function
}
pub fn build_zVXMSTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXMSTYPE");
    function
}
pub fn build_zVXMTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXMTYPE");
    function
}
pub fn build_zVXSG() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXSG");
    function
}
pub fn build_zVXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zVXTYPE");
    function
}
pub fn build_zWFI() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWFI");
    function
}
pub fn build_zWMVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWMVVTYPE");
    function
}
pub fn build_zWMVXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWMVXTYPE");
    function
}
pub fn build_zWRS() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWRS");
    function
}
pub fn build_zWVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWVTYPE");
    function
}
pub fn build_zWVVTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWVVTYPE");
    function
}
pub fn build_zWVXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWVXTYPE");
    function
}
pub fn build_zWXTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zWXTYPE");
    function
}
pub fn build_zXPERM4() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zXPERM4");
    function
}
pub fn build_zXPERM8() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zXPERM8");
    function
}
pub fn build_zZBA_RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBA_RTYPE");
    function
}
pub fn build_zZBA_RTYPEUW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBA_RTYPEUW");
    function
}
pub fn build_zZBB_EXTOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBB_EXTOP");
    function
}
pub fn build_zZBB_RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBB_RTYPE");
    function
}
pub fn build_zZBB_RTYPEW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBB_RTYPEW");
    function
}
pub fn build_zZBKB_PACKW() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBKB_PACKW");
    function
}
pub fn build_zZBKB_RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBKB_RTYPE");
    function
}
pub fn build_zZBS_IOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBS_IOP");
    function
}
pub fn build_zZBS_RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZBS_RTYPE");
    function
}
pub fn build_zZCMOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZCMOP");
    function
}
pub fn build_zZICBOM() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZICBOM");
    function
}
pub fn build_zZICBOP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZICBOP");
    function
}
pub fn build_zZICBOZ() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZICBOZ");
    function
}
pub fn build_zZICOND_RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZICOND_RTYPE");
    function
}
pub fn build_zZILX_LOAD() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZILX_LOAD");
    function
}
pub fn build_zZIMOP_MOP_R() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZIMOP_MOP_R");
    function
}
pub fn build_zZIMOP_MOP_RR() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZIMOP_MOP_RR");
    function
}
pub fn build_zZIP() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZIP");
    function
}
pub fn build_zZVABD_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVABD_VV");
    function
}
pub fn build_zZVABD_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVABD_VX");
    function
}
pub fn build_zZVKSHA2TYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVKSHA2TYPE");
    function
}
pub fn build_zZVKSM4RTYPE() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVKSM4RTYPE");
    function
}
pub fn build_zZVWABDA_VV() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVWABDA_VV");
    function
}
pub fn build_zZVWABDA_VX() -> Function {
    let mut function = model().functions["zexecute"].clone();
    vmlab_hv_tcg_ir::specialize_constructor(&mut function, "zZVWABDA_VX");
    function
}
pub const INSTRUCTION_TAGS: &[&str] = &[
    "zADDIW",
    "zAES32DSI",
    "zAES32DSMI",
    "zAES32ESI",
    "zAES32ESMI",
    "zAES64DS",
    "zAES64DSM",
    "zAES64ES",
    "zAES64ESM",
    "zAES64IM",
    "zAES64KS1I",
    "zAES64KS2",
    "zAMO",
    "zBITYPE",
    "zBREV8",
    "zBTYPE",
    "zCLMUL",
    "zCLMULH",
    "zCLMULR",
    "zCLZ",
    "zCLZW",
    "zCPOP",
    "zCPOPW",
    "zCSRImm",
    "zCSRReg",
    "zCTZ",
    "zCTZW",
    "zC_ADD",
    "zC_ADDI",
    "zC_ADDI16SP",
    "zC_ADDI4SPN",
    "zC_ADDIW",
    "zC_ADDW",
    "zC_AND",
    "zC_ANDI",
    "zC_BEQZ",
    "zC_BNEZ",
    "zC_EBREAK",
    "zC_FLD",
    "zC_FLDSP",
    "zC_FLW",
    "zC_FLWSP",
    "zC_FSD",
    "zC_FSDSP",
    "zC_FSW",
    "zC_FSWSP",
    "zC_ILLEGAL",
    "zC_J",
    "zC_JAL",
    "zC_JALR",
    "zC_JR",
    "zC_LBU",
    "zC_LD",
    "zC_LDSP",
    "zC_LH",
    "zC_LHU",
    "zC_LI",
    "zC_LUI",
    "zC_LW",
    "zC_LWSP",
    "zC_MUL",
    "zC_MV",
    "zC_NOP",
    "zC_NOT",
    "zC_NTL",
    "zC_OR",
    "zC_SB",
    "zC_SD",
    "zC_SDSP",
    "zC_SEXT_B",
    "zC_SEXT_H",
    "zC_SH",
    "zC_SLLI",
    "zC_SRAI",
    "zC_SRLI",
    "zC_SSPOPCHK",
    "zC_SSPUSH",
    "zC_SUB",
    "zC_SUBW",
    "zC_SW",
    "zC_SWSP",
    "zC_XOR",
    "zC_ZEXT_B",
    "zC_ZEXT_H",
    "zC_ZEXT_W",
    "zDIV",
    "zDIVW",
    "zEBREAK",
    "zECALL",
    "zFCVTMOD_W_D",
    "zFCVT_BF16_S",
    "zFCVT_S_BF16",
    "zFENCE",
    "zFENCEI",
    "zFENCE_TSO",
    "zFLEQ_D",
    "zFLEQ_H",
    "zFLEQ_S",
    "zFLI_D",
    "zFLI_H",
    "zFLI_S",
    "zFLTQ_D",
    "zFLTQ_H",
    "zFLTQ_S",
    "zFMAXM_D",
    "zFMAXM_H",
    "zFMAXM_S",
    "zFMINM_D",
    "zFMINM_H",
    "zFMINM_S",
    "zFMVH_X_D",
    "zFMVP_D_X",
    "zFROUNDNX_D",
    "zFROUNDNX_H",
    "zFROUNDNX_S",
    "zFROUND_D",
    "zFROUND_H",
    "zFROUND_S",
    "zFVFMATYPE",
    "zFVFMTYPE",
    "zFVFTYPE",
    "zFVVMATYPE",
    "zFVVMTYPE",
    "zFVVTYPE",
    "zFWFTYPE",
    "zFWVFMATYPE",
    "zFWVFTYPE",
    "zFWVTYPE",
    "zFWVVMATYPE",
    "zFWVVTYPE",
    "zF_BIN_F_TYPE_D",
    "zF_BIN_F_TYPE_H",
    "zF_BIN_RM_TYPE_D",
    "zF_BIN_RM_TYPE_H",
    "zF_BIN_RM_TYPE_S",
    "zF_BIN_TYPE_F_S",
    "zF_BIN_TYPE_X_S",
    "zF_BIN_X_TYPE_D",
    "zF_BIN_X_TYPE_H",
    "zF_MADD_TYPE_D",
    "zF_MADD_TYPE_H",
    "zF_MADD_TYPE_S",
    "zF_UN_F_TYPE_D",
    "zF_UN_F_TYPE_H",
    "zF_UN_RM_FF_TYPE_D",
    "zF_UN_RM_FF_TYPE_H",
    "zF_UN_RM_FF_TYPE_S",
    "zF_UN_RM_FX_TYPE_D",
    "zF_UN_RM_FX_TYPE_H",
    "zF_UN_RM_FX_TYPE_S",
    "zF_UN_RM_XF_TYPE_D",
    "zF_UN_RM_XF_TYPE_H",
    "zF_UN_RM_XF_TYPE_S",
    "zF_UN_TYPE_F_S",
    "zF_UN_TYPE_X_S",
    "zF_UN_X_TYPE_D",
    "zF_UN_X_TYPE_H",
    "zHFENCE_GVMA",
    "zHFENCE_VVMA",
    "zHINVAL_GVMA",
    "zHINVAL_VVMA",
    "zHLVTYPE",
    "zHSV",
    "zILLEGAL",
    "zITYPE",
    "zJAL",
    "zJALR",
    "zLOAD",
    "zLOADRES",
    "zLOAD_FP",
    "zLPAD",
    "zMASKTYPEI",
    "zMASKTYPEV",
    "zMASKTYPEX",
    "zMMTYPE",
    "zMOVETYPEI",
    "zMOVETYPEV",
    "zMOVETYPEX",
    "zMRET",
    "zMUL",
    "zMULW",
    "zMVVCOMPRESS",
    "zMVVMATYPE",
    "zMVVTYPE",
    "zMVXMATYPE",
    "zMVXTYPE",
    "zNISTYPE",
    "zNITYPE",
    "zNTL",
    "zNVSTYPE",
    "zNVTYPE",
    "zNXSTYPE",
    "zNXTYPE",
    "zORCB",
    "zPAUSE",
    "zREM",
    "zREMW",
    "zREV8",
    "zRFVVTYPE",
    "zRFWVVTYPE",
    "zRIVVTYPE",
    "zRMVVTYPE",
    "zRORI",
    "zRORIW",
    "zRTYPE",
    "zRTYPEW",
    "zSFENCE_INVAL_IR",
    "zSFENCE_VMA",
    "zSFENCE_W_INVAL",
    "zSHA256SIG0",
    "zSHA256SIG1",
    "zSHA256SUM0",
    "zSHA256SUM1",
    "zSHA512SIG0",
    "zSHA512SIG0H",
    "zSHA512SIG0L",
    "zSHA512SIG1",
    "zSHA512SIG1H",
    "zSHA512SIG1L",
    "zSHA512SUM0",
    "zSHA512SUM0R",
    "zSHA512SUM1",
    "zSHA512SUM1R",
    "zSHIFTIOP",
    "zSHIFTIWOP",
    "zSINVAL_VMA",
    "zSLLIUW",
    "zSM3P0",
    "zSM3P1",
    "zSM4ED",
    "zSM4KS",
    "zSRET",
    "zSSAMOSWAP",
    "zSSPOPCHK",
    "zSSPUSH",
    "zSSRDP",
    "zSTORE",
    "zSTORECON",
    "zSTORE_FP",
    "zUNZIP",
    "zUTYPE",
    "zVAESDF",
    "zVAESDM",
    "zVAESEF",
    "zVAESEM",
    "zVAESKF1_VI",
    "zVAESKF2_VI",
    "zVAESZ_VS",
    "zVANDN_VV",
    "zVANDN_VX",
    "zVBREV8_V",
    "zVBREV_V",
    "zVCLMULH_VV",
    "zVCLMULH_VX",
    "zVCLMUL_VV",
    "zVCLMUL_VX",
    "zVCLZ_V",
    "zVCPOP_M",
    "zVCPOP_V",
    "zVCTZ_V",
    "zVEXTTYPE",
    "zVFIRST_M",
    "zVFMERGE",
    "zVFMV",
    "zVFMVFS",
    "zVFMVSF",
    "zVFNCVTBF16_F_F_W",
    "zVFNUNARY0",
    "zVFUNARY0",
    "zVFUNARY1",
    "zVFWCVTBF16_F_F_V",
    "zVFWMACCBF16_VF",
    "zVFWMACCBF16_VV",
    "zVFWUNARY0",
    "zVGHSH_VV",
    "zVGMUL_VV",
    "zVICMPTYPE",
    "zVID_V",
    "zVIMCTYPE",
    "zVIMSTYPE",
    "zVIMTYPE",
    "zVIOTA_M",
    "zVISG",
    "zVITYPE",
    "zVLRETYPE",
    "zVLSEGFFTYPE",
    "zVLSEGTYPE",
    "zVLSSEGTYPE",
    "zVLXSEGTYPE",
    "zVMSBF_M",
    "zVMSIF_M",
    "zVMSOF_M",
    "zVMTYPE",
    "zVMVRTYPE",
    "zVMVSX",
    "zVMVXS",
    "zVREV8_V",
    "zVROL_VV",
    "zVROL_VX",
    "zVROR_VI",
    "zVROR_VV",
    "zVROR_VX",
    "zVSETIVLI",
    "zVSETVL",
    "zVSETVLI",
    "zVSHA2MS_VV",
    "zVSM3C_VI",
    "zVSM3ME_VV",
    "zVSM4K_VI",
    "zVSRETYPE",
    "zVSSEGTYPE",
    "zVSSSEGTYPE",
    "zVSXSEGTYPE",
    "zVVCMPTYPE",
    "zVVMCTYPE",
    "zVVMSTYPE",
    "zVVMTYPE",
    "zVVTYPE",
    "zVWSLL_VI",
    "zVWSLL_VV",
    "zVWSLL_VX",
    "zVXCMPTYPE",
    "zVXMCTYPE",
    "zVXMSTYPE",
    "zVXMTYPE",
    "zVXSG",
    "zVXTYPE",
    "zWFI",
    "zWMVVTYPE",
    "zWMVXTYPE",
    "zWRS",
    "zWVTYPE",
    "zWVVTYPE",
    "zWVXTYPE",
    "zWXTYPE",
    "zXPERM4",
    "zXPERM8",
    "zZBA_RTYPE",
    "zZBA_RTYPEUW",
    "zZBB_EXTOP",
    "zZBB_RTYPE",
    "zZBB_RTYPEW",
    "zZBKB_PACKW",
    "zZBKB_RTYPE",
    "zZBS_IOP",
    "zZBS_RTYPE",
    "zZCMOP",
    "zZICBOM",
    "zZICBOP",
    "zZICBOZ",
    "zZICOND_RTYPE",
    "zZILX_LOAD",
    "zZIMOP_MOP_R",
    "zZIMOP_MOP_RR",
    "zZIP",
    "zZVABD_VV",
    "zZVABD_VX",
    "zZVKSHA2TYPE",
    "zZVKSM4RTYPE",
    "zZVWABDA_VV",
    "zZVWABDA_VX",
];
