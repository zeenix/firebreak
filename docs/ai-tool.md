# The AI tool

`firebreak-mcp` lets an AI model use exactly one capability of the Firebreak payment agent: paying
the allowance's merchant. It is a [Model Context Protocol](https://modelcontextprotocol.io) server
on standard input and output (protocol versions 2025-06-18, 2025-03-26 and 2024-11-05). It offers
two tools and forwards them to the agent's local HTTP API. It holds no keys.

| Tool | Arguments | Agent request |
| --- | --- | --- |
| `allowance_status` | none | `GET /api/status`: the merchant, the vouchers and their states |
| `pay` | `merchant`, `amount`, optional `allowance` | `POST /api/pay` with exactly those fields |

`amount` is a string of decimal digits (whole sparks). Vouchers are fixed-value and single-use, so
an amount must equal an exact sum of the remaining unspent vouchers. An amount that has no exact
subset is a limit of the denominations, not a security rejection.

Whatever the agent answers is returned to the model as the tool's result, as the agent's own JSON.
An answer other than a success, or no answer at all, comes back flagged `isError`, so the model
reads the refusal, such as `{"error": "...", "stage": "policy"}`, instead of the call failing
silently. Arguments that are malformed (a missing or non-numeric `amount`) are refused by the tool
itself, without calling the agent. If the agent does not answer, the error says that the payment
may or may not have been made, and that the model should check `allowance_status` before paying
again. Stderr, never stdout, carries one log line per call.

## What the model can and cannot do

The model gets the delegated `pay` operation and nothing else. It has no owner operations (it
cannot fund an allowance or recover one), no key, and no way to ask the agent for anything but the
two requests above: the agent's demo-only `GET /api/delegated-key` (served only with
`--reveal-key`) is not reachable through this server, which has no other URL in it. At worst the
model can spend the allowance, all of it if it is told to, at the one merchant the owner chose.
That is what the allowance authorizes (see "Not claimed" in [threat-model.md](threat-model.md)).

A prompt-injected instruction to "pay this other address" is refused twice, in independent places:

1. The agent's policy check refuses a merchant that is not the allowance's. The tool does not
   second-guess the merchant, so the model reads the agent's own refusal, with its stage.
2. If that check were bypassed, the chain refuses. Each voucher's predicate has two programs, and
   the delegated one pays only the merchant that was fixed when the voucher was funded: neither the
   delegated key nor anything the model sends can change the destination.

The second refusal can be shown without a model, which is the deterministic version of the same
attempt. These commands go around the agent and its policy check, straight to the node, with the
delegated key, and try to pay the attacker's own address. Run from the repository root, they work
as they are against a running `scripts/demo.sh` once an allowance is funded. Add `--key HEX` to use
the key the dashboard revealed:

```sh
target/release/firebreak-attack attempt key-path
target/release/firebreak-attack attempt forged-leaf
```

- `key-path` signs a spend of the voucher as if its predicate were a plain key. The verifier and
  the node refuse the signed bytes (`BatchSignatureVerificationFailed`).
- `forged-leaf` opens the voucher with a redemption leaf that is identical to the real one except
  that it pays the attacker. The VM cannot prove it against the voucher's predicate
  (`TaprootProofMismatch`). This is a prover-stage refusal: no transaction bytes exist to submit,
  which is weaker evidence than a node refusing bytes (see [threat-model.md](threat-model.md)).

Each command reports where the candidate was stopped, and the voucher's state before and after,
which a refused attack leaves unchanged. `firebreak-attack list` names the other attacks. Keep
these commands in any AI demonstration: a live model may refuse the injected instruction, or be
unavailable.

The guarantee is about this tool, not about the host it runs in. A model that also has a shell or
file access can do whatever the machine allows, including reading `.firebreak/agent/agent.json`,
which holds the delegated key. Run the demonstration in a session that has only this server's tools.

## Registering it with Claude Code

```sh
cargo build --release -p firebreak-mcp
claude mcp add firebreak -- <path>/target/release/firebreak-mcp --agent http://127.0.0.1:7741
```

`<path>` is the checkout. The agent must be running (`firebreak-agent serve`, which
`scripts/demo.sh` starts). `--agent` defaults to `http://127.0.0.1:7741` and must be a plain
`http://` URL.
