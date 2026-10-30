# Licensing

Polymorph is **dual-licensed**:

1. **Open source:** [AGPL-3.0-only](../LICENSE) for everyone. Running it, modifying
   it, and self-hosting it are free.
2. **Commercial:** a separate licence for embedding Polymorph in a proprietary
   product or offering it as part of a commercial service. A dedicated commercial
   address will be published here; until then, open an issue at
   <https://github.com/walkthestars/polymorph> and it will be routed.

This file explains what the AGPL means at the point of install. It is not legal
advice; if the distinction matters to your organisation, ask your counsel.

## What AGPL-3.0-only allows without asking

- Install and run the binary for any purpose, including inside a company.
- Use it in CI, in an internal orchestrator, or behind an internal service whose
  users are all inside the same organisation.
- Modify it for private use, with no obligation to publish your changes.
- Redistribute it, provided the whole work is licensed under AGPL-3.0-only and
  the source is made available.

## What needs the commercial licence

- **Embedding** Polymorph (or a derivative) in a product you distribute under
  terms other than AGPL-3.0-only.
- **Offering it as a network service** to third parties where you do not want the
  AGPL §13 source-offer obligation to cover your service code.
- Any use where an enterprise dependency allowlist rejects AGPL. That rejection
  is expected: most default `cargo-deny` policies and many corporate allowlists
  deny AGPL outright. The commercial licence exists for exactly that case; if you
  need it, ask before wiring Polymorph into a product.

## Why the project can dual-license

Every contribution needs both of the following (see `../CONTRIBUTING.md`):

- a **DCO** `Signed-off-by` trailer on each commit (`../DCO`), and
- a one-time acceptance of the **CLA** (`../CLA.md`), whose §2 grants the Project
  Owner a perpetual, worldwide, irrevocable, sublicensable copyright licence and
  whose §3 adds a patent grant.

Without those inbound rights the Project Owner could not offer the commercial
licence, so contributions that do not carry them cannot be merged.

## Release metadata

Every release carries `license = "AGPL-3.0-only"` (SPDX) in `Cargo.toml` and in
each crate's metadata. A release is the pair `vX.Y.Z` tag + crates.io packages +
GitHub release; the licence shown in the README at that tag is the licence that
applies to that release.
