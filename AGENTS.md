# Repository Guidelines

## Project Structure & Module Organization

- `crates/` contains the Rust workspace: `server` (API and orchestration), `execution` (sandbox hosts and agents), `core` (client state and rendering), `domain` (authorization), `proto` (protocol bindings), and `bridge` (Flutter FFI).
- `client/lib/` holds Flutter UI; `client/test/` contains widget tests and `goldens/`; fonts live in `client/assets/fonts/`.
- `proto/` defines wire schemas; `migrations/` holds numbered SQL migrations; `harness/claude/` contains the TypeScript adapter. Development scripts live in `scripts/` and the development stack in `compose.yaml`; `deploy/` holds the self-hosting setup and `docs/` the documentation.

## Build, Test, and Development Commands

Use Rust ≥1.94, Docker Compose, and Flutter 3.47.5/Dart 3.13.4. Run commands from the repository root unless specified; initialize Flutter dependencies with `(cd client && flutter pub get)`.

- `cargo build --workspace --locked`: build Rust crates.
- `./scripts/dev.sh`: start PostgreSQL/NATS, migrate, and run the server on port 8080 against a local model fixture.
- `cargo fmt --all --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings`: enforce Rust lint checks.
- `cargo test --workspace --locked`: run non-ignored Rust tests.
- `./scripts/test-client.sh`: build the native bridge, check Dart formatting, analyze Dart, and run Flutter tests.
- `./scripts/build-web.sh`, then `python3 scripts/serve-web.py`: build and serve Web on port 3000; install the Web prerequisites documented in `README.md` first.

## Coding Style & Naming Conventions

Use rustfmt defaults (four-space indentation), `snake_case` Rust functions/modules, and `UpperCamelCase` types. Format Dart with `dart format client/lib client/test`; use two-space indentation, `lower_snake_case.dart` filenames, and `lowerCamelCase` members. Follow `flutter_lints` from `client/analysis_options.yaml`.

## Testing Guidelines

Rust uses `#[test]` and `#[tokio::test]`, with unit tests beside source and integration suites in crate `tests/` directories. Name tests descriptively; Flutter files use `*_test.dart` and `flutter_test`.

Server integration tests are ignored by default: run `./scripts/test-integration.sh` and relevant `test-platform.sh`, `test-work.sh`, `test-harnesses.sh`, or `test-search.sh` scripts under `scripts/`. They provision temporary databases; execution suites also require Linux/container runtimes. Real-provider checks run only when `RYZHUB_TEST_PROVIDER_KEY` and the URL/model variables are set (scripts read `.env`). No coverage percentage is enforced. Add regression coverage for changed behavior; review intentional golden-image changes.

## Commit & Pull Request Guidelines

Commits and PRs should follow the Conventional Commits specification.

## Generated Files & Configuration

Regenerate bridge bindings through `flutter_rust_bridge.yaml`. After schema or checked-query changes, run `DATABASE_URL=... ./scripts/prepare-sql.sh` against a migrated database and commit `.sqlx/` updates. Keep credentials in ignored `.env`/`var/` files; `.env.example` is for local development.

## Code Quality Restrictions

### When editing code

- Use red/green TDD when adding features or fixing issues.
- No over-engineering: no overly defensive design, no unnecessary implementation, and no features added without the maintainer's permission.
- No duplicate or unnecessary tests, but ensure at least one test passes before finishing a task.
- Minimize mocking; test under real conditions where viable, and ask the maintainer for keys or other information when needed.
- Think twice before creating or editing a Markdown file; avoid unnecessary text in both documents and the user interface.
- Always follow clean code principles.

### When exploring

- Be open-minded; feel free to explore different approaches.

### When reviewing code

- Do not adopt the author's mindset; keep asking whether each implementation is necessary.
- Do not hold back on the assumption that something "might be the maintainer's specific requirement"; point out anything you consider inappropriate.

### When asked for an implementation plan

- Think through the details and state the decisions you made in the final plan.

### When writing documents

- Be brief but informative: maximize information density while keeping the text readable and quick to understand.

### When asked to commit changes

- Split the edits into several smaller commits if necessary.
