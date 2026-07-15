// Guard against silently losing `-C target-cpu=native` (carried only by
// `.cargo/config.toml`'s [build] rustflags, which ANY exported RUSTFLAGS overrides —
// cargo's rustflags sources are mutually exclusive). Zen 4 native and the sanctioned
// x86-64-v3 fallback both set avx2; baseline x86-64 does not, so this fires exactly
// when the flag was dropped. Deliberately-portable builds opt out via the
// `allow-baseline-cpu` feature.
#[cfg(all(
    target_arch = "x86_64",
    not(target_feature = "avx2"),
    not(feature = "allow-baseline-cpu"),
    not(doctest)
))]
compile_error!(
    "built without -C target-cpu=native/x86-64-v3: RUSTFLAGS likely overrode .cargo/config.toml; \
     re-add the flag (compose it into RUSTFLAGS) or enable the `allow-baseline-cpu` feature"
);

pub mod snapshot;
pub mod eligibility;
pub mod engine;

pub use snapshot::Snapshot;
pub use engine::{EngineView, SearchParams, SearchResult, SearchStatus, NeighborProvider, LandmarkHeuristic};