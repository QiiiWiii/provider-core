# provider-core

A local Rust service that exposes Codex- and Claude-compatible APIs for upstream AI providers.

## Workspace

- `crates/provider-core`: stable provider, account, and proxy contracts
- `crates/provider-protocol`: downstream wire protocol conversion
- `crates/provider-drivers`: built-in upstream provider drivers
- `crates/provider-runtime`: live accounts and credential refresh coordination
- `crates/provider-storage`: SQLx and SQLite persistence
- `crates/provider-server`: Axum HTTP server and process composition

## Containers

The default `Dockerfile` builds the all-in-one image: `provider-core` serves
both the API and the compiled `provider-ui` SPA on port `8317`.

## Runtime configuration

The inference capacity settings are read at startup. If unset, the production
defaults are 10 executing requests, 20 additional queued requests per account,
and a 30-second queue wait:

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PROVIDER_INFERENCE_CONCURRENCY` | `10` | Concurrent requests per account |
| `PROVIDER_INFERENCE_QUEUE_CAPACITY` | `20` | Additional waiting requests per account |
| `PROVIDER_INFERENCE_QUEUE_TIMEOUT_SECONDS` | `30` | Maximum queue wait in seconds |

Invalid values fail startup instead of silently changing the capacity policy.

## OpenAI-compatible upstreams

Inbound `x-opencode-session` metadata is validated and preserved for
OpenAI-compatible upstream requests. Accounts using the normalized base URL
`https://opencode.ai/zen/go/v1` use the `opencode-go` models.dev entries for
model pricing, so prices reflect that upstream instead of another provider
that happens to publish the same model ID.

Every OpenAI-compatible account declares its native `upstream_protocol` as
`responses` or `chat_completions`. A Chat Completions account can serve an
inbound Responses request through the stateless protocol bridge. The client
must send complete history; stateful continuations, provider-hosted tools,
custom tool grammars, and other fields without a lossless Chat Completions
representation are rejected before dispatch. A Chat Completions upstream does
not produce opaque encrypted reasoning state; readable reasoning is returned
as a Responses summary and must be included in that complete history.

~~~bash
docker buildx build --load -t provider-core .
docker buildx build --load --build-arg UI_REF=<branch-tag-or-commit> -t provider-core .
~~~

For a backend-only image, build the `core-runtime` target. It does not require
the UI checkout:

~~~bash
docker buildx build --load --target core-runtime -t provider-core:core .
~~~

~~~bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
~~~
