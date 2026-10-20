# NWC wallet plugin

Reuse the mesh-llm wallet.v1 contract; keep the backend in a reusable library and the binary thin. No host ledger or budget policy here. Never expose payment operations as MCP tools.

`src/nwc.rs` is the only module that touches NIP-47 types. Use `just check` and `just test`; Clippy runs with warnings denied.

Tests must not contact live wallets or relays, move funds, or read real connection secrets. `live_payment_round_trip` is the only exception: it is `#[ignore]`d and needs explicit authorization plus both `NWC_TEST_*` variables.

Never log or echo the NWC URI; it carries the connection secret. Preserve uncertainty after payment submission: classify an error as not-submitted only for wallet refusals that happen before any payment attempt, and never blindly retry a send.

Keep `mesh-llm-plugin`, `mesh-llm-wallet` and `mesh-llm-plugin-manager` on the same mesh-llm git `rev`.
