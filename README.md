<h1 align="center">Sanctuary</h1>

<p align="center">
  <strong>The Rust community API behind Illyaverse accounts, beatmaps, scores and player data.</strong>
</p>

Sanctuary provides the data and permission layer for
[Illyaverse](https://github.com/Illyavrs/Illyaverse). It serves the Serendipity
website and supplies shared account, beatmap and score services to Resonance.

## Responsibilities

- Accounts, sessions, recovery, roles and permissions.
- Profiles, player preferences, uploaded media and social relationships.
- Beatmap discovery, imports, packages, ranking requests and staff review.
- Scores, replays, PP calculations, leaderboards and rank history.
- Website chat, moderation, announcements and private audit records.

Sanctuary checks authentication, account restrictions and required permissions
at the API boundary. It uses Rust, Axum, Tokio, SQLx and PostgreSQL, with
`rosu-pp` for performance calculations and shared storage for media, beatmap
packages and replays.

Public website routes use `/api/v1/*` and `/media/*`. Resonance handles stable
endpoints and packets, lazer OAuth and API v2, and game realtime connections;
it calls Sanctuary's authenticated internal routes for shared data operations.
See the [architecture](https://github.com/Illyavrs/Illyaverse/blob/main/apps/docs/content/docs/deploy/architecture.md) and
[protocol compatibility](https://github.com/Illyavrs/Illyaverse/blob/main/apps/docs/content/docs/reference/protocol-compatibility.md) guides for the
service boundary and verification scope.

## Build and run

Use a complete Illyaverse parent checkout and Rust **1.96**. The parent owns
`Cargo.toml`, `Cargo.lock`, `crates/illya-domain`,
`crates/resonance-protocol` and `migrations/`; this child repository alone
does not include the complete build or deployment environment.

Run these commands from the **Illyaverse root**:

```sh
cargo build -p sanctuary --locked
cargo test -p sanctuary --locked
cargo clippy -p sanctuary --all-targets --locked -- -D warnings
```

For host-side development, configure PostgreSQL, `DATABASE_URL`,
`SESSION_SECRET` (at least 32 bytes), the shared `INTERNAL_SERVICE_KEY` and
the deployment settings described in the [architecture guide](https://github.com/Illyavrs/Illyaverse/blob/main/apps/docs/content/docs/deploy/architecture.md),
then run `cargo run -p sanctuary --locked`. Sanctuary applies the parent
migrations before listening. Database integration tests require an isolated
PostgreSQL fixture.

For the complete local deployment, follow the
[parent setup](https://github.com/Illyavrs/Illyaverse/blob/main/README.md#run-a-local-server), then run from the parent root:

```sh
docker compose up -d --build --wait
```

The Docker build context is the parent root. Sanctuary listens on container
port **8080**; the default Compose stack keeps that port inside the network.
`GET /health/live` checks the process and `GET /health/ready` checks database
availability. Use the [deployment guide](https://github.com/Illyavrs/Illyaverse/blob/main/apps/docs/content/docs/deploy/deployment.md) for HTTPS,
environment settings and persistent storage.

## Contributing

Source: [Illyavrs/Sanctuary](https://github.com/Illyavrs/Sanctuary).
Use scoped Conventional Commits such as `feat(sanctuary): ...` and
`fix(sanctuary): ...`, and follow [AGENTS.md](AGENTS.md).

Sanctuary is original software under the
[MIT License](https://github.com/Illyavrs/Illyaverse/blob/main/LICENSE).
