# pithos-grammar-corrector

Automatic grammar correction for pithos/pi user prompts.

The extension intercepts user input, corrects spelling and grammar using a configured model, shows a colored diff, and submits the corrected prompt automatically without confirmation.

## Install

From a local checkout:

```bash
pi install ./pithos-grammar-corrector
```

Project-local install:

```bash
pi install ./pithos-grammar-corrector -l
```

Temporary test run:

```bash
pi -e ./pithos-grammar-corrector
```

## Pithos `.pithos` config

This package currently lives inside the `anton-kochev/pithos` repository. Reference that repository with a pinned ref:

```yaml
pi:
  extensions:
    pithos-grammar-corrector: "git:https://github.com/anton-kochev/pithos.git#v0.1.0"
```

A branch also works during development:

```yaml
pi:
  extensions:
    pithos-grammar-corrector: "git:https://github.com/anton-kochev/pithos.git#main"
```

The repository root `index.ts` re-exports this extension so pithos can clone the repository into pi's global extensions directory and pi can auto-discover it.

## Configuration

Create `.pi/grammar-corrector.json` in your project:

```json
{
  "mode": "on",
  "model": "openai-codex/gpt-5.4-mini",
  "maxInputChars": 500
}
```

Options:

- `mode`: `"on"` or `"off"`
- `model`: pi model spec in `provider/model` format
- `maxInputChars`: maximum input length to send to the correction model

Environment variables override the config file:

```bash
GRAMMAR_CORRECTOR_MODE=off pi
GRAMMAR_CORRECTOR_MODEL=openai-codex/gpt-5.4-mini pi
GRAMMAR_CORRECTOR_MAX_CHARS=1000 pi
```

## Status

Inside pi:

```text
/grammar-corrector-status
```

## Notes

This package imports pi runtime packages as peer dependencies:

- `@earendil-works/pi-ai`
- `@earendil-works/pi-coding-agent`

Do not bundle those dependencies; pi provides them at runtime.
