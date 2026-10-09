#!/bin/bash

RUN_FILE="/run/bwmon.state"
STATE_DIR="/run/user/$UID/bwmon"
STATE_FILE="${STATE_DIR}/bwmon.state"
SMA_WINDOW="${SMA_WINDOW:-4}"

# Validate SMA_WINDOW is a positive integer, fallback to 4
# Keep as a multiple of bwmon-publisher's interval setting
# to avoid oscillations in calculations
if [[ ! "$SMA_WINDOW" =~ ^[0-9]+$ ]] || [ "$SMA_WINDOW" -lt 1 ]; then
  SMA_WINDOW=4
fi

# Ensure our user-specific state directory exists
if [[ ! -d "$STATE_DIR" ]]; then
  mkdir -p "$STATE_DIR" || exit 1
fi

if [[ ! -r "$RUN_FILE" ]]; then
  echo "⚠ No Data | color=red"
  echo "---"
  echo "Publisher service not running or $RUN_FILE missing."
  exit 0
fi

TOTAL_RX_B=0
TOTAL_TX_B=0
DROPDOWN_OUTPUT=""
NEW_STATE_BUFFER=""

format_3sigfig() {
  local bytes=${1:-0}
  local out_var=$2
  local suffix=$3

  # Direct byte formatting for <= 999 bytes (e.g. 0 B, 45 B, 999 B/s)
  if [ "$bytes" -le 999 ]; then
    [ "$bytes" -lt 0 ] && bytes=0
    printf -v "$out_var" "%d B%s" "$bytes" "$suffix"
    return
  fi

  local u_val u_name

  if [ "$bytes" -lt 1048576 ]; then
    u_val=1024
    u_name="KiB"
  elif [ "$bytes" -lt 1073741824 ]; then
    u_val=1048576
    u_name="MiB"
  elif [ "$bytes" -lt 1099511627776 ]; then
    u_val=1073741824
    u_name="GiB"
  else
    u_val=1099511627776
    u_name="TiB"
  fi

  local int=$(( bytes / u_val ))
  local rem=$(( bytes % u_val ))

  if [ "$int" -lt 10 ]; then
    # 1 integer digit -> 2 decimal places (e.g. 1.45 TiB)
    local frac=$(( (rem * 100 + u_val / 2) / u_val ))
    if [ "$frac" -ge 100 ]; then
      int=$(( int + 1 ))
      frac=0
    fi
    if [ "$int" -ge 10 ]; then
      printf -v "$out_var" "%d.%d %s%s" "$int" "$(( frac / 10 ))" "$u_name" "$suffix"
    else
      printf -v "$out_var" "%d.%02d %s%s" "$int" "$frac" "$u_name" "$suffix"
    fi
  elif [ "$int" -lt 100 ]; then
    # 2 integer digits -> 1 decimal place (e.g. 14.5 TiB)
    local frac=$(( (rem * 10 + u_val / 2) / u_val ))
    if [ "$frac" -ge 10 ]; then
      int=$(( int + 1 ))
      frac=0
    fi
    if [ "$int" -ge 100 ]; then
      printf -v "$out_var" "%d %s%s" "$int" "$u_name" "$suffix"
    else
      printf -v "$out_var" "%d.%d %s%s" "$int" "$frac" "$u_name" "$suffix"
    fi
  else
    # 3+ integer digits -> 0 decimal places (e.g. 145 TiB)
    local rounded=$(( (bytes + u_val / 2) / u_val ))
    printf -v "$out_var" "%d %s%s" "$rounded" "$u_name" "$suffix"
  fi
}

format_speed() {
  format_3sigfig "$1" "$2" "/s"
}

format_bytes() {
  format_3sigfig "$1" "$2" ""
}

# Load all previous states into memory at once
declare -A PREV_MAP
if [[ -r "$STATE_FILE" ]]; then
  # State format: netns device rx1 tx1 rx2 tx2 ... rxN txN
  while read -r p_netns p_device p_hist; do
    PREV_MAP["${p_netns}_${p_device}"]="$p_hist"
  done < "$STATE_FILE"
fi

# Read the live data line by line
while read -r netns device RX TX; do
  [[ -z "$netns" ]] && continue

  # Validate RX and TX are strictly numbers
  if [[ ! "$RX" =~ ^[0-9]+$ ]] || [[ ! "$TX" =~ ^[0-9]+$ ]]; then
    continue
  fi

  KEY="${netns}_${device}"

  # Retrieve previous state from memory
  # p_hist contains: rx1 tx1 rx2 tx2 ... rxN txN (1 is 1 sec ago, 2 is 2 secs ago...)
  read -r -a hist_arr <<< "${PREV_MAP[$KEY]}"

  # Find the oldest valid point for our SMA window
  oldest_rx=$RX
  oldest_tx=$TX
  span=0

  for ((w = SMA_WINDOW; w >= 1; w--)); do
    idx_rx=$(( (w - 1) * 2 ))
    idx_tx=$(( idx_rx + 1 ))
    if [[ "${hist_arr[idx_rx]}" =~ ^[0-9]+$ ]] && [[ "${hist_arr[idx_tx]}" =~ ^[0-9]+$ ]]; then
      oldest_rx=${hist_arr[idx_rx]}
      oldest_tx=${hist_arr[idx_tx]}
      span=$w
      break
    fi
  done

  # Calculate Speeds (Average over the span)
  if [ "$span" -eq 0 ]; then
    RX_SPEED=0
    TX_SPEED=0
  else
    RX_SPEED=$(( (RX - oldest_rx) / span ))
    TX_SPEED=$(( (TX - oldest_tx) / span ))
  fi

  [ "$RX_SPEED" -lt 0 ] && RX_SPEED=0
  [ "$TX_SPEED" -lt 0 ] && TX_SPEED=0

  # Append to the new state buffer to write to disk later
  # We shift the history to the right, using '-' as a placeholder if a value is missing
  new_hist=("$RX" "$TX")
  for ((i = 0; i < (SMA_WINDOW - 1) * 2; i += 2)); do
    new_hist+=("${hist_arr[i]:--}" "${hist_arr[i+1]:--}")
  done
  NEW_STATE_BUFFER+="$netns $device ${new_hist[*]}\n"

  # Exclude VPN devices to prevent double-counting encapsulated traffic
  if [[ "$device" != wg* ]] && [[ "$device" != tun* ]] && [[ "$device" != tap* ]]; then
    TOTAL_RX_B=$((TOTAL_RX_B + RX_SPEED))
    TOTAL_TX_B=$((TOTAL_TX_B + TX_SPEED))
  fi

  # Format individual speeds directly into variables
  format_speed "$RX_SPEED" FMT_RX_SPEED
  format_speed "$TX_SPEED" FMT_TX_SPEED

  # Format cumulative counters (auto-scales KiB/MiB/GiB/TiB)
  format_bytes "$RX" FMT_RX_TOTAL
  format_bytes "$TX" FMT_TX_TOTAL

  case "$device" in
    wg*|tun*|tap*) ICON="🔒" ;;  # WireGuard / VPN
    en*|eth*)      ICON="🔌" ;;  # Ethernet
    wl*)           ICON="📡" ;;  # Wi-Fi
    lo)            ICON="🔄" ;;  # Loopback
    *)             ICON="🌐" ;;  # Default / Other
  esac

  INDENT="&#160;&#160;&#160;&#160;"
  DROPDOWN_OUTPUT+="<span>$ICON [$netns] <b>$device</b></span> | useMarkup=true\n"
  DROPDOWN_OUTPUT+="${INDENT}<span font_family='monospace'>↓ ${FMT_RX_SPEED}  ↑ ${FMT_TX_SPEED}</span> | useMarkup=true\n"
  DROPDOWN_OUTPUT+="${INDENT}<span font_family='monospace'>RX: ${FMT_RX_TOTAL}  TX: ${FMT_TX_TOTAL}</span> | useMarkup=true\n"

done < "$RUN_FILE"

# Save all states to disk
printf -v NEW_STATE_OUTPUT "%b" "$NEW_STATE_BUFFER"
echo "$NEW_STATE_OUTPUT" > "$STATE_FILE"

# Format total speeds
format_speed "$TOTAL_RX_B" FMT_TOT_RX
format_speed "$TOTAL_TX_B" FMT_TOT_TX

# --- ARGOS OUTPUT ---
echo "↓ ${FMT_TOT_RX}  ↑ ${FMT_TOT_TX} | font=monospace"
echo "---"
echo -e "$DROPDOWN_OUTPUT"
