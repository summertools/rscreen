#!/bin/bash
# Demo content used by the screenshot generator. The first pane shows a
# project view; subsequent panes show a test run. This is intentionally just a
# static script so screenshots are deterministic.
count=$(flock /tmp/rscreen-demo.lock bash -c 'n=$(cat /tmp/rscreen-demo.count 2>/dev/null || echo 0); n=$((n+1)); echo $n > /tmp/rscreen-demo.count; echo $n')
clear
if [ "$count" = "1" ]; then
  printf '\033[1;38;2;203;166;247m  rscreen\033[0m  \033[2m/ workspace\033[0m\n'
  printf '\033[38;2;69;71;90m  ────────────────────────────────────────\033[0m\n\n'
  printf '  \033[1mPROJECT\033[0m\n\n'
  printf '  \033[38;2;137;180;250m▸\033[0m \033[38;2;137;180;250msrc/\033[0m\n'
  for f in main.rs client.rs session.rs render.rs input.rs copy.rs; do
    printf '    \033[38;2;205;214;244m%s\033[0m\n' "$f"
  done
  printf '  \033[38;2;137;180;250m▸\033[0m Cargo.toml\n'
  printf '  \033[38;2;137;180;250m▸\033[0m README.md\n\n'
  printf '  \033[1mRECENT\033[0m\n\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  polish the status bar\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  smooth copy mode\n'
  printf '  \033[38;2;249;226;175m●\033[0m  split panes\n\n'
  printf '  \033[2mCtrl-a c  new window   Ctrl-a ?  help\033[0m\n'
else
  printf '\033[1;38;2;205;214;244m  TEST RUN\033[0m  \033[2m·  just now\033[0m\n'
  printf '\033[38;2;69;71;90m  ────────────────────────────────────────\033[0m\n\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  terminal emulator       42 tests\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  pane layout             12 tests\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  protocol                 9 tests\n'
  printf '  \033[38;2;166;227;161m✓\033[0m  copy mode               15 tests\n\n'
  printf '  \033[38;2;166;227;161m42 passed\033[0m \033[2m· 0 failed · 1.24s\033[0m\n\n'
  printf '  \033[2mwaiting for changes…\033[0m\n'
fi
exec sleep 3600
