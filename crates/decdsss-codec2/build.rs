// made by CasualArclamp
//! Compiles the vendored Codec2 core (`c/`, see its README in the crate root).

fn main() {
    let c = std::path::Path::new("c");
    let mut b = cc::Build::new();
    for f in [
        "lpc",
        "nlp",
        "postfilter",
        "sine",
        "codec2",
        "codec2_fft",
        "kiss_fft",
        "kiss_fftr",
        "interp",
        "lsp",
        "mbest",
        "newamp1",
        "newamp2",
        "phase",
        "quantise",
        "pack",
        "golay23",
        "codebook",
        "codebookd",
        "codebookjmv",
        "codebookge",
        "codebooknewamp1",
        "codebooknewamp1_energy",
        "codebooknewamp2",
        "codebooknewamp2_energy",
    ] {
        b.file(c.join(format!("{f}.c")));
    }
    b.include(c)
        // M_PI and friends on MSVC.
        .define("_USE_MATH_DEFINES", None)
        // Codec2's own sources are not ours to tidy.
        .warnings(false)
        .opt_level(3);
    b.compile("codec2");
    println!("cargo:rerun-if-changed=c");
    if std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f == "unix") {
        println!("cargo:rustc-link-lib=m");
    }
}
