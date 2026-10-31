# Pinned Sail model ISA configuration

This is the output of `cargo run -p vmlab-hv-tcg --example isa` for
sail-riscv fc074f99f578954376214c4ca9490152d35d0ec2 with
`rv64d_v128_e64.json`. The query sets mstatus.FS and mstatus.VS to Dirty
before calling the generated `currentlyEnabled` predicate for each extension.
The base I instruction set is listed explicitly because it is not an
extension-enum member. Privilege and translation entries are retained.

```text
I M A F D B V S U H
Sha Shcounterenw Shgatpa Shlcofideleg Shtvala Shvsatpa Shvstvala Shvstvecd Zic64b Zicbom
Zicbop Zicboz Zicfiss Zicntr Zicond Zicsr Zifencei Zihintntl Zihintpause Zihpm
Zimop Zmmul Zaamo Zabha Zacas Zalrsc Zama16b Zawrs Za64rs Za128rs
Zfa Zfbfmin Zfh Zfhmin Zca Zcb Zcd Zcmop C Zba
Zbb Zbc Zbkb Zbkc Zbkx Zbs Ziccamoa Ziccamoc Ziccif Zicclsm
Ziccrse Zknd Zkne Zknh Zkr Zksed Zksh Zkt Zvl32b Zvl64b
Zvl128b Zve32f Zve32x Zve64d Zve64f Zve64x Zvfbfmin Zvfbfwma Zvfh Zvfhmin
Zvbb Zvbc Zvkb Zvkg Zvkned Zvknha Zvknhb Zvksed Zvksh Zvkt
Ssccptr Sscofpmf Sscounterenw Ssnpm Ssstateen Ssstrict Sdtrig Sstc Sstvala Sstvecd
Ssu64xl Svbare Sv39 Sv48 Sv57 Svade Svadu Svinval Svnapot Svpbmt
Svrsw60t59b Svvptc Smcntrpmf Smmpm Smnpm Smstateen Ssqosid Sspm Supm
```

The flat-image corpus validates RV64GC, Zicsr, Zifencei, and the tested M/S
privileged behavior. Other entries have generated semantics but are not
validated by that corpus. Zkr also requires a host entropy provider, which
this package does not implement. This list describes the pinned model; it
must not be used as an unqualified VMM capability advertisement.
