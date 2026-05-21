#!/usr/bin/env bash
# Pixel-art P-51 Mustang diving down-left — 4-terminal-row variant using quadrant block pixels (8x16 pixels).

color_for() {
  case "$1" in
    R) echo 196 ;;
    r) echo 124 ;;
    O) echo 208 ;;
    Y) echo 220 ;;
    K) echo  16 ;;
    B) echo  45 ;;
    b) echo  33 ;;
    N) echo  18 ;;
    T) echo  94 ;;
    G) echo  34 ;;
    g) echo  22 ;;
    S) echo 250 ;;
    s) echo 244 ;;
    W) echo 255 ;;
    *) echo  "" ;;
  esac
}

plane=(
  "....WW.........."
  "....WW......RKSs"
  ".....WW...SSSs.."
  ".....WWSSSSs...."
  "......SSSS......"
  "....bBSWW......."
  "..YOOSs.WW......"
  "YO.......WW....."
)

draw_cell() {
  local ul=$1 ur=$2 ll=$3 lr=$4
  local uniq=""
  for c in "$ul" "$ur" "$ll" "$lr"; do
    [ "$c" = "." ] && continue
    case "$uniq" in *"$c"*) ;; *) uniq+="$c" ;; esac
  done
  if [ -z "$uniq" ]; then
    printf " "
    return
  fi
  local A=${uniq:0:1}
  local p=""
  [ "$ul" = "$A" ] && p+="1" || p+="0"
  [ "$ur" = "$A" ] && p+="1" || p+="0"
  [ "$ll" = "$A" ] && p+="1" || p+="0"
  [ "$lr" = "$A" ] && p+="1" || p+="0"
  local fg=$(color_for "$A")
  local ch
  case "$p" in
    1111) ch="█" ;;
    1110) ch="▛" ;;
    1101) ch="▜" ;;
    1011) ch="▙" ;;
    0111) ch="▟" ;;
    1100) ch="▀" ;;
    0011) ch="▄" ;;
    1010) ch="▌" ;;
    0101) ch="▐" ;;
    1001) ch="▚" ;;
    0110) ch="▞" ;;
    1000) ch="▘" ;;
    0100) ch="▝" ;;
    0010) ch="▖" ;;
    0001) ch="▗" ;;
  esac
  if [ ${#uniq} -ge 2 ]; then
    local B=${uniq:1:1}
    local bg=$(color_for "$B")
    printf "\e[38;5;%s;48;5;%sm%s\e[0m" "$fg" "$bg" "$ch"
  else
    printf "\e[38;5;%sm%s\e[0m" "$fg" "$ch"
  fi
}

echo
n=${#plane[@]}
width=${#plane[0]}
for (( y=0; y<n; y+=2 )); do
  top="${plane[$y]}"
  bot="${plane[$((y+1))]}"
  printf "  "
  for (( x=0; x<width; x+=2 )); do
    draw_cell "${top:x:1}" "${top:$((x+1)):1}" "${bot:x:1}" "${bot:$((x+1)):1}"
  done
  printf "\n"
done
echo

