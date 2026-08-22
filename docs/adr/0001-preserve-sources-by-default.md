# Preserve source images by default

Downscaling permanently discards pixel detail even when conversion and replacement are technically flawless. The default mode therefore writes verified reductions to a separate output tree, while irreversible source replacement requires the explicit `--replace` option and confirmation.
