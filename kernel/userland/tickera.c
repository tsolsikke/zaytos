/* 起こしっぱなしの 1 本（W1-c-4）。本体は `ticker.h`。
 *
 * **`B` より長く回す**——**`B` の `spawn` の会計は空きフレームの大域の差で閉じるので、
 * `A` の破棄がその間に入ると合わなくなる**（`docs/wayland-inventory.md` の #4）。 */
#define TICKER_NAME 'A'
#define TICKER_STEP 1.25
#define TICKER_ROUNDS 24UL
#define TICKER_FOLDS
#include "ticker.h"
