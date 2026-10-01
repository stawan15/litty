#!/usr/bin/env bash
# Scripted demo for recording the README GIF:  litty -e bash assets/demo.sh
cd "$(dirname "$0")/.." || exit 1
LITTY=${LITTY:-./target/release/litty}

demo() {  # demo <typed text> <code to run> [pause]
  printf '\033[1;32m❯\033[0m '
  for ((i = 0; i < ${#1}; i++)); do printf '%s' "${1:i:1}"; sleep 0.03; done
  sleep 0.2; echo
  eval "$2"; echo; sleep "${3:-0.8}"
}

clear
demo "litty --about" "echo 'litty: a small, fast terminal'"

demo "colors" 'for i in {0..7}; do printf "\033[4${i}m   \033[0m"; done; echo; for i in {8..15}; do printf "\033[48;5;${i}m   \033[0m"; done; echo'

demo "truecolor" 'for i in $(seq 0 3 255); do printf "\033[48;2;%d;%d;%dm \033[0m" $i $((255-i)) 200; done; echo'

demo "styles" 'printf "\033[1mbold\033[0m \033[3mitalic\033[0m \033[4munderline\033[0m \033[9mstrike\033[0m \033[4:3;58;2;255;80;80mcurly\033[0m\n"'

demo "unicode" 'echo "สวัสดีครับ — ภาษาไทยแสดงผลถูกต้อง  🚀 ✨ 日本語"'

demo "build" 'for p in $(seq 0 5 100); do printf "\033]9;4;1;%d\a" $p; printf "\r building… %3d%%" $p; sleep 0.04; done; printf "\033]9;4;0\a\n"'

[ -x "$LITTY" ] && demo "litty img logo.png" "$LITTY img packaging/litty.png" 2

demo "brew install --cask stawan15/tap/litty" "echo 'zero config.'" 2
