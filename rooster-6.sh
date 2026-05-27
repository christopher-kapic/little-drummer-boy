#!/usr/bin/env bash
color_for() {
  case "$1" in
    a) echo   0 ;;
    b) echo   1 ;;
    c) echo   2 ;;
    d) echo   3 ;;
    e) echo   4 ;;
    f) echo   6 ;;
    g) echo   7 ;;
    h) echo   8 ;;
    i) echo   9 ;;
    j) echo  11 ;;
    k) echo  12 ;;
    l) echo  15 ;;
    *) echo  "" ;;
  esac
}

plane=(
  ".........ii........................."
  "........iiaii......................."
  ".......jjiegb..........eeehhhha....."
  ".........ebbddd......ce.eeeeeee....."
  ".........ebdddd.....aeeeeeeee..e...."
  "........eebbdjjjh..llgheeeegeece...."
  "........eeeeaabbbibiighheeeeee.c...."
  "........eeeeeeiiibbejjdeeeeeke.e...."
  "..........eeeeeeeeedbb.eeec.k......."
  ".............eeeeedddb.e....f......."
  "..............eaeae................."
  "...............d.d.................."
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

