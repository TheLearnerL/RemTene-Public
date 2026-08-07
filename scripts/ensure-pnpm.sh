#!/bin/sh
# Ensure pnpm is available, using corepack if needed

if command -v pnpm >/dev/null 2>&1; then
  # pnpm already in PATH
  exit 0
fi

if ! command -v corepack >/dev/null 2>&1; then
  echo "Error: Neither pnpm nor corepack is available" >&2
  exit 1
fi

# Prepare pnpm via corepack
corepack prepare pnpm@11.9.0 --activate >/dev/null 2>&1 || true

# Create a temporary wrapper script
temp_dir=$(mktemp -d)
cat > "$temp_dir/pnpm" << 'EOF'
#!/bin/sh
exec corepack pnpm "$@"
EOF
chmod +x "$temp_dir/pnpm"

echo "$temp_dir"
