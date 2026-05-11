<p align="center"><strong>Codex CLI (Fork)</strong> is a modified version of the coding agent from OpenAI.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>

> [!IMPORTANT]
> **This is a fork.** Key changes:
> - **Realtime disabled:** Realtime features and handlers are disabled.
> - **Hardened Config:** Session and config are hardened to use local defaults; overrides are disabled.
> - **No Collaboration Mode:** Collaboration mode APIs and updates have been removed.

---

## Quickstart

### Build and Install from source

Clone the repository and build using Cargo:

```shell
git clone https://github.com/openai/codex.git
cd codex/codex-rs

# Build the release binary
cargo build --release
```

After building, you can find the binary at `target/release/codex`. You can move it to your path:

```shell
cp target/release/codex /usr/local/bin/
```

Then simply run `codex` to get started.

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
