# Repository file tree summary

```text
.
├── .cargo/
├── .config/
├── .github/
├── AGENTS.md
├── Cargo.toml
├── Justfile
├── README.md
├── agent/
├── apps/
│   └── web/
├── config/
├── configs/
├── crates/
│   ├── jeryu-api/
│   ├── jeryu-cli/
│   └── jeryu-split-tool/
├── db/
├── docs/
├── examples/
├── images/
├── ops/
├── policies/
├── scripts/
├── tools/
└── tests/
```

The root `Cargo.toml` enrolls exactly the three listed product crates. External
Jeryu components remain immutable Git dependencies; absent monorepo directories
are not synthesized as local workspace members.
