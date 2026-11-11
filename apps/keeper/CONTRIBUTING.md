# Contributing to the Agentroid keeper (apps/keeper)

A small, working tool, tested on one phone so far, so results from other devices are the most useful thing you can give it. Small, concrete contributions are welcome.

## Ways to help
- **Report a result.** Tried it on a different device, OS version or setup? Open an issue with what you ran and what you saw, even if it worked.
- **Report a bug.** Use the bug template: what you did, what you expected, what happened, and the exact command and version.
- **Suggest something.** Open a Discussion first for anything bigger than a few lines, so we agree on the shape before you write it.
- **Send a pull request.** Keep it to one change. Say what it fixes and how you checked it.

## Before a pull request
- Read the repository's [CONTRIBUTING.md](../../CONTRIBUTING.md) too: every commit needs a DCO sign-off (`git commit -s`).
- Run the check from `apps/keeper`: `mkdir -p out/test && javac -d out/test src/dev/agentroid/keeper/KeeperLogic.java test/KeeperLogicTest.java && java -cp out/test KeeperLogicTest`
- No secrets, tokens, device serials, personal addresses or private data in code, tests, logs or screenshots.
- Keep the dependency count where it is. This project aims to stay small.
- Match the style of the file you are editing.

## Licence
By contributing you agree your contribution is released under this folder's MIT licence (see `LICENSE` here).
