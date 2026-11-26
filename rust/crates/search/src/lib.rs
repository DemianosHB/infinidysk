//! Search-result domain logic, ported from the C# v1 fork (`backend/Utils/`):
//! language-token parsing and subtitle ranking ([`language`]), audio-language
//! detection and the preferred-language ordering bias ([`audio`], the fork's
//! "preferred audio languages" feature), release quality parsing ([`quality`]) and
//! the profile result ordering ([`sorter`]).
//!
//! Pure and side-effect free; no I/O. The bias is an ordering bias, never a filter:
//! releases whose language cannot be detected confidently are never demoted.

pub mod audio;
pub mod language;
pub mod quality;
pub mod sorter;
