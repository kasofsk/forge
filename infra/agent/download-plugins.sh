#!/bin/sh
# download-plugins.sh — fetch Claude Code plugins from GitHub marketplace repos.
#
# Reads a config file where each line is: github_owner/repo:plugin1,plugin2
# Empty plugin list means install all plugins from that marketplace.
# Downloads plugin files into TARGET_DIR/{plugin_name}/.
#
# Usage: download-plugins.sh <config_file> <target_dir>

set -e

CONFIG="$1"
TARGET="$2"

if [ -z "$CONFIG" ] || [ -z "$TARGET" ]; then
    echo "Usage: $0 <config_file> <target_dir>" >&2
    exit 1
fi

if [ ! -f "$CONFIG" ]; then
    echo "Config file not found: $CONFIG" >&2
    exit 1
fi

mkdir -p "$TARGET"

while IFS= read -r line || [ -n "$line" ]; do
    # Skip comments and empty lines
    case "$line" in
        ''|\#*) continue ;;
    esac

    # Parse marketplace_repo:plugins
    repo="${line%%:*}"
    plugins_str="${line#*:}"
    # If no colon was present, plugins_str equals the whole line (meaning install all)
    if [ "$plugins_str" = "$line" ]; then
        plugins_str=""
    fi

    echo "Fetching marketplace: $repo"
    marketplace_url="https://raw.githubusercontent.com/${repo}/main/.claude-plugin/marketplace.json"
    marketplace=$(curl -fsSL "$marketplace_url") || {
        echo "  Warning: failed to fetch $marketplace_url, skipping" >&2
        continue
    }

    # Determine which plugins to install
    if [ -z "$plugins_str" ]; then
        # Install all plugins
        plugin_names=$(echo "$marketplace" | jq -r '.plugins[].name')
    else
        # Install specified plugins
        plugin_names=$(echo "$plugins_str" | tr ',' '\n')
    fi

    for plugin in $plugin_names; do
        [ -z "$plugin" ] && continue

        # Get the source path from marketplace.json
        source_path=$(echo "$marketplace" | jq -r ".plugins[] | select(.name==\"$plugin\") | .source")
        if [ -z "$source_path" ] || [ "$source_path" = "null" ]; then
            echo "  Warning: plugin '$plugin' not found in $repo marketplace, skipping" >&2
            continue
        fi

        # Normalize source path (remove leading ./)
        source_path="${source_path#./}"
        base_url="https://raw.githubusercontent.com/${repo}/main/${source_path}"

        echo "  Installing plugin: $plugin"
        plugin_dir="$TARGET/$plugin"
        mkdir -p "$plugin_dir/.claude-plugin"

        # Download plugin.json
        curl -fsSL "$base_url/.claude-plugin/plugin.json" \
            -o "$plugin_dir/.claude-plugin/plugin.json" 2>/dev/null || {
            echo "    Warning: failed to download plugin.json for $plugin" >&2
            continue
        }

        # Download hooks if present
        if curl -fsSL "$base_url/hooks/hooks.json" -o /dev/null 2>/dev/null; then
            mkdir -p "$plugin_dir/hooks"
            curl -fsSL "$base_url/hooks/hooks.json" -o "$plugin_dir/hooks/hooks.json"

            # Download hook scripts referenced in hooks.json
            # Extract script filenames from command fields
            scripts=$(jq -r '.. | .command? // empty' "$plugin_dir/hooks/hooks.json" \
                | sed 's|.*/${CLAUDE_PLUGIN_ROOT}/hooks/||' \
                | sed 's|.*/||' \
                | sort -u)
            for script in $scripts; do
                [ -z "$script" ] && continue
                curl -fsSL "$base_url/hooks/$script" -o "$plugin_dir/hooks/$script" 2>/dev/null && \
                    chmod +x "$plugin_dir/hooks/$script" || \
                    echo "    Warning: failed to download hook $script" >&2
            done
        fi

        # Download skills if present (check for skill dirs by listing)
        # Skills are optional — skip if not available
        if curl -fsSL "$base_url/skills/" -o /dev/null 2>/dev/null; then
            mkdir -p "$plugin_dir/skills"
            # GitHub raw doesn't support directory listing, so we parse marketplace
            # for hints. Skills are typically named after the plugin.
        fi

        echo "    ✓ $plugin"
    done
done < "$CONFIG"

echo "Plugins installed to $TARGET"
