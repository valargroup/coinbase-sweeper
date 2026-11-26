# Source this before running the regtest tests:  . scripts/regtest-env.sh
# Set ZAKURAD_BIN (zakurad, tag v1.8.8) and ZECD_BIN (zecd with mining-pool support) first.
: "${ZAKURAD_BIN:?set ZAKURAD_BIN to your zakurad binary}"
: "${ZECD_BIN:?set ZECD_BIN to your zecd binary}"
export ZAKURAD_BIN ZECD_BIN
export ZECD_REGTEST_NODE=zakura
export ZECD_REGTEST_NU63_HEIGHT=8
export ZECD_REGTEST_NU7_HEIGHT=12
