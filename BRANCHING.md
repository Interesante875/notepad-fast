# Branching model

| Branch | Purpose | CI |
| --- | --- | --- |
| `feature/*`, `claude/*` | Day-to-day development of individual changes. | lint, tests on Linux/Windows/macOS, cross-compiles |
| `develop` | Integration branch for development. Feature branches merge here first. | same as above |
| `staging` | Release candidate. Merge `develop` here to produce test builds. | + release binaries for all platforms (workflow artifacts) |
| `production` | What users run. Merge `staging` here to ship. | + publishes a GitHub Release with all binaries |
| `main` | Default branch; mirrors `production`. | lint and tests |

Flow:

```
feature/xyz ──► develop ──► staging ──► production (──► main)
```

Tagging a commit `vX.Y.Z` also publishes a release with that name.
