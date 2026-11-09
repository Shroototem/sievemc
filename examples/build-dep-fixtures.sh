#!/bin/sh
# Rebuild the dependency-resolution test fixtures from their Java sources.
#
#   examples/deps-src/          -> examples/deps/           (Fabric)
#   examples/deps-neoforge-src/ -> examples/deps-neoforge/  (NeoForge)
#
# Stub classes (net.fabricmc.api.*, net.minecraft.core.Registry,
# net.neoforged.*, com.mojang.brigadier.*) are compile-time only and are never
# packed into a fixture jar. Requires a JDK 21+ (`javac` and `jar`).
#
# Usage: sh examples/build-dep-fixtures.sh   (from the repository root)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

compile() { # <source dir> <class output dir>
    mkdir -p "$2"
    (cd "$1" && javac --release 21 -d "$2" $(find . -name '*.java'))
}

# <out dir> <jar name> <class package dir> <file in jar, or -> <file text> [<MANIFEST.MF text>]
pack() {
    stage="$work/stage-$2"
    rm -rf "$stage"
    mkdir -p "$stage/$3"
    cp "$work/classes/$3"/*.class "$stage/$3/"
    if [ "$4" != - ]; then
        mkdir -p "$(dirname "$stage/$4")"
        printf '%s' "$5" > "$stage/$4"
    fi
    rm -f "$1/$2.jar"
    if [ $# -ge 6 ]; then
        # jar ignores a final manifest line without a newline; $(...) strips it.
        printf '%s\n' "$6" > "$work/MANIFEST-$2.MF"
        (cd "$stage" && jar --create --file "$1/$2.jar" --manifest "$work/MANIFEST-$2.MF" .)
    else
        (cd "$stage" && jar --create --file "$1/$2.jar" .)
    fi
}

# ---- Fabric -----------------------------------------------------------------
out="$root/examples/deps"
mkdir -p "$out"
rm -rf "$work/classes"
compile "$root/examples/deps-src" "$work/classes"
fabric() { pack "$out" "$1" "$2" fabric.mod.json "$3"; }

# A client-flagged config library that servermod calls from its `main` code.
fabric configlib cfg '{"schemaVersion":1,"id":"configlib","version":"1","entrypoints":{"client":["cfg.ClientInit::init"]},"custom":{"modmenu":{"badges":["library"]}}}'
# Reached only through configlib (library -> library chain).
fabric deeplib deeplib '{"schemaVersion":1,"id":"deeplib","version":"1","environment":"*"}'
# Used only from servermod's @Environment(CLIENT) code.
fabric uilib ui '{"schemaVersion":1,"id":"uilib","version":"1","environment":"*"}'
# A regular mod servermod touches only through an undeclared, guarded call.
fabric othermod othermod '{"schemaVersion":1,"id":"othermod","version":"1","environment":"*","entrypoints":{"main":["othermod.Api::init"]}}'
# Registers content; recommends configlib and uilib.
fabric servermod sm '{"schemaVersion":1,"id":"servermod","version":"1","environment":"*","entrypoints":{"main":["sm.Main::init"]},"recommends":{"configlib":"*","uilib":"*"}}'
# Shaped like architectury: a library with entrypoints that nothing uses.
fabric orphanlib orphan '{"schemaVersion":1,"id":"orphanlib","version":"1","environment":"*","entrypoints":{"main":["orphan.Init::init"],"server":["orphan.Init::server"]},"depends":{"uilib":"*"},"custom":{"modmenu":{"badges":["library"]}}}'
# Used by clientmod without being declared anywhere.
fabric hiddenlib hidden '{"schemaVersion":1,"id":"hiddenlib","version":"1","environment":"*","custom":{"modmenu":{"badges":["library"]}}}'
fabric clientmod cm '{"schemaVersion":1,"id":"clientmod","version":"1","environment":"client","entrypoints":{"client":["cm.ClientMain::init"]}}'

# ---- NeoForge ---------------------------------------------------------------
out="$root/examples/deps-neoforge"
mkdir -p "$out"
rm -rf "$work/classes"
compile "$root/examples/deps-neoforge-src" "$work/classes"
toml() { # <mod id> <modLoader>
    printf 'modLoader="%s"\nloaderVersion="[1,)"\nlicense="MIT"\n[[mods]]\nmodId="%s"\nversion="1"\n[[dependencies.%s]]\nmodId="neoforge"\ntype="required"\nside="BOTH"\n' "$2" "$1" "$1"
}
neo() { pack "$out" "$1" "$2" META-INF/neoforge.mods.toml "$(toml "$1" "$3")"; }

neo neo_cmd neocmd javafml                 # @Mod registers a command -> both
neo neo_idle neoidle javafml               # @Mod does nothing server-side -> client
neo neo_clientdist neoclient javafml       # @Mod(dist = CLIENT) -> client
neo neo_scripted neoscripted neo_lang      # needs the neo_lang language provider
neo neo_methodref neomethodref javafml     # server work only via `Setup::init`

# neo_lang: a language provider wrapper (no mods.toml), found via modLoader.
pack "$out" neo_lang neolang META-INF/services/net.neoforged.neoforgespi.language.IModLanguageLoader 'neolang.Provider' \
    "$(printf 'Manifest-Version: 1.0\nFMLModType: LANGPROVIDER\nAutomatic-Module-Name: neo_lang\n')"
# neo_unusedlib: a library wrapper nothing uses -> unused.
pack "$out" neo_unusedlib neounused - '' \
    "$(printf 'Manifest-Version: 1.0\nFMLModType: GAMELIBRARY\nAutomatic-Module-Name: neo_unusedlib\n')"

echo "Rebuilt $(ls "$root/examples/deps" | wc -l) Fabric and $(ls "$root/examples/deps-neoforge" | wc -l) NeoForge fixtures."
