#![cfg(feature = "sandbox")]

//! Additional behavioural tests for simulacra-tool coverage gaps.
//!
//! Covers: FT1 (SkillTool surface), FT2 (skill frontmatter parsing),
//! FT6 (file_edit error paths), FT7 (list_dir on file), FT8 (capability denial
//! for file_write, shell_exec, list_dir), GFT3 (list_dir missing path),
//! GFT5 (file_write budget exhaustion via max_turns).

include!("parts/coverage_gaps_00.rs");
include!("parts/coverage_gaps_01.rs");
include!("parts/coverage_gaps_02.rs");
include!("parts/coverage_gaps_03.rs");
