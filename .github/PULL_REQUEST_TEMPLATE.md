**What changed, and why**

**How you tested it** (emulator, a physical device with make and Android version, or unit tests only)

**Checks run** (see CONTRIBUTING.md): `cargo fmt`, `clippy`, `cargo test`, `npm test`, others:

- [ ] Every commit is signed off (`git commit -s`; check with `python scripts/check_dco.py main..HEAD`).
- [ ] No serial numbers, real IP addresses, tokens or personal screenshots.
- [ ] If this widens what an agent can do, there is a test for the new behaviour and one showing the nearest forbidden neighbour is still refused.
