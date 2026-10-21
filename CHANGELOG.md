# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `World`, with `spawn`, `spawn_batch`, `despawn`, `insert`, `remove`, `get`,
  `get_mut`, `has`, `contains`, `len` and `clear`.
- The `Component` trait, and the `Bundle` trait implemented for components
  and for nested tuples of up to 12 bundles.
- `Entity` handles with generations, convertible to and from `u64`.
- The `NoSuchEntity` and `RemoveError` error types.
