# sievemc

A Rust workspace for detecting and filtering Minecraft mod JARs by side — client-only, server-only, or both. Supports Fabric, NeoForge, and Forge mod formats.

## Workspace crates

| Crate                                 | Description                                                                                            |
|---------------------------------------|--------------------------------------------------------------------------------------------------------|
| [`sievemc_lib`](crates/lib/README.md) | Core library — inspect a mod JAR and determine its side                                                |
| [`sievemc_cli`](crates/cli/README.md) | Command-line tool — scan a directory and output results to the terminal, a directory, or a zip archive |

## Supported mod loaders

| Loader   | Detection method                                                                              |
|----------|-----------------------------------------------------------------------------------------------|
| Fabric   | `fabric.mod.json` — `environment` field and/or `entrypoints` keys                             |
| NeoForge | `META-INF/neoforge.mods.toml` or `neoforge.mods.toml` — `dependencies.<modId>[neoforge].side` |
| Forge    | `META-INF/mods.toml` or `mods.toml` — `dependencies.<modId>[forge].side`                      |

### Dependency-aware sides

A manifest only *suggests* a side (and NeoForge's `side = "BOTH"` is usually template boilerplate). After the per-jar pass, sievemc answers two questions for every mod:

1. **Does another mod require it?** Declared dependencies (and the `modLoader` language provider) are enforced by the loader, so a required mod goes on every side its dependent is on. Code use counts too: a mod calling into a library needs that library wherever it runs.
2. **Does its own code do anything on the server?** One bytecode walk starts from the code that loads on a dedicated server (entry points not marked client-only, mixins not aimed at client code). If it registers game content, commands or network packets, runs server code, patches non-client game code, or the jar ships data packs, the mod runs on the server; otherwise it's client-only.

Libraries go wherever the mods using them go. A library nothing uses — or that another jar already bundles in full — is reported as **unused** and left out of both exports. Each result records who requires it, on which side, and why (`required_by`, plus the reason in `evidence`).

## Quickstart

```sh
# Print all mods with their detected side
sievemc_cli /path/to/mods

# Copy client-only mods to a directory
sievemc_cli --output-type directory --output ./out --side client /path/to/mods

# Create separate client.zip and server.zip
sievemc_cli --output-type archive --output ./out --side both /path/to/mods
```
