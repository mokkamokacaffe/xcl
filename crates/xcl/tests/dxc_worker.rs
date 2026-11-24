//! `XCL_DXC_PROCESS`: compiles in worker processes under a memory cap. A compile past the cap
//! ends its worker and fails; the compiles after it get a new worker and succeed.

use xcl::gpu::recomp::{dxc, runner};

const SMALL: &str = "RWByteAddressBuffer o : register(u0);\n[numthreads(64,1,1)] void main(uint3 t : SV_DispatchThreadID) { o.Store(t.x * 4, t.x * 3u); }";

/// A compute program in the shape of a translated one: registers as locals, a `pc` switch of
/// `blocks` cases. DXC's memory grows with it.
fn big_source(blocks: u32) -> String {
    let regs = 96u32;
    let mut s = String::from("RWByteAddressBuffer o : register(u0);\n[numthreads(64,1,1)] void main(uint3 t : SV_DispatchThreadID) {\n");
    for i in 0..regs {
        s += &format!("uint v{i} = t.x * {}u;\n", i + 1);
    }
    s += "uint pc = 0; uint budget = 100000;\n[loop] while (pc != 0xffffffffu && budget-- > 0) { switch (pc) {\n";
    let mut x = 1u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x % regs
    };
    for b in 0..blocks {
        s += &format!("case {b}u: {{\n");
        for k in 0..8 {
            let (d, a, c) = (next(), next(), next());
            s += &match k % 4 {
                0 => format!("v{d} = v{a} + v{c};\n"),
                1 => format!("v{d} = select(v{a} > v{c}, v{a} - v{c}, v{c} * 3u);\n"),
                2 => format!("v{d} = (v{a} & 0xffffu) * (v{c} >> 16);\n"),
                _ => format!("v{d} = asuint(asfloat(v{a}) * 0.5f + asfloat(v{c}));\n"),
            };
        }
        let to = if b + 1 < blocks { b + 1 } else { 0xffff_ffff };
        s += &format!("pc = (v{} & 1u) ? {to}u : 0xffffffffu; break; }}\n", next());
    }
    s += "default: pc = 0xffffffffu; break; } }\no.Store(t.x * 4, ";
    s += &(0..regs).map(|i| format!("v{i}")).collect::<Vec<_>>().join(" ^ ");
    s += ");\n}\n";
    s
}

fn dxil(r: &Result<Vec<u8>, String>) -> bool {
    r.as_ref().is_ok_and(|b| b.starts_with(b"DXBC"))
}

#[test]
fn a_compile_past_the_cap_ends_its_worker_and_the_next_one_gets_a_new_worker() {
    if dxc::location().is_err() {
        runner::skip_test("no DXC");
        return;
    }
    let cap_mb: u64 = std::env::var("XCL_TEST_DXC_CAP_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(24);
    std::env::set_var("XCL_DXC_WORKER_EXE", env!("CARGO_BIN_EXE_xcl"));
    std::env::set_var("XCL_DXC_PROCESS", cap_mb.to_string());
    let small = dxc::compile(SMALL, "main", "cs_6_6", &[]);
    assert!(dxil(&small), "a small program in the worker: {:?}", small.as_ref().err());
    println!("after a small program: worker peak {} MB", xcl::gpu::recomp::dxc_process::peak().unwrap_or(0) >> 20);
    let blocks: u32 = std::env::var("XCL_TEST_DXC_BLOCKS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let src = big_source(blocks);
    let t = std::time::Instant::now();
    let big = dxc::compile(&src, "main", "cs_6_6", &[]);
    println!("{} KB source, {cap_mb} MB cap: {:.1} s, {:?}; worker peak {} MB", src.len() / 1024, t.elapsed().as_secs_f32(), big.as_ref().map(|b| b.len()), xcl::gpu::recomp::dxc_process::peak().unwrap_or(0) >> 20);
    let e = match big {
        Ok(b) => panic!("a program past the {cap_mb} MB cap compiled ({} KB DXIL)", b.len() / 1024),
        Err(e) => e,
    };
    assert!(e.contains("cap"), "{e}");
    // this process is unharmed, and the next compile gets a new worker
    let again = dxc::compile(SMALL, "main", "cs_6_6", &[]);
    assert!(dxil(&again), "after a worker ended: {:?}", again.as_ref().err());
}
