# Bundled speech model

Sotto ships the speech model **inside the application package**. The user never
downloads anything at runtime.

Expected files here (from `moondream/parakeet-redux`):

| File | Size |
|---|---|
| `model.safetensors` | 177,774,490 B |
| `tokenizer.json` | 1,159,960 B |
| `ternary.json` | 57,970 B |
| `config.json` | 12,988 B |

Run `scripts/fetch-model.sh` to populate this directory. The contents are
git-ignored because they are large binaries, but the *build* bundles whatever is
present here, so a release artifact always contains them.
