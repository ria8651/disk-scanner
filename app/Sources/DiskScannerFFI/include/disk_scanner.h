// C ABI for disk-scanner. Mirrors `src/ffi.rs` — change both together.
//
// Layout drift is caught at compile time by the _Static_asserts at the bottom;
// the expected sizes come from the `abi::layout` test in src/ffi.rs.

#ifndef DISK_SCANNER_H
#define DISK_SCANNER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// --- enums -----------------------------------------------------------------

enum { DS_KIND_DIR = 0, DS_KIND_FILE = 1, DS_KIND_SYMLINK = 2, DS_KIND_OTHER = 3 };

enum {
  DS_STATE_OK = 0,
  DS_STATE_DENIED_TCC = 1,   // TCC refused; Full Disk Access would fix it
  DS_STATE_DENIED_PERM = 2,  // ownership/mode; FDA would NOT help
  DS_STATE_ERROR = 3,
  DS_STATE_SKIPPED = 4
};

enum { DS_ACCESS_FULL = 0, DS_ACCESS_DENIED = 1, DS_ACCESS_UNKNOWN = 2 };

enum {
  DS_SORT_PHYSICAL = 0,
  DS_SORT_EXCLUSIVE = 1,
  DS_SORT_LOGICAL = 2,
  DS_SORT_NAME = 3,
  DS_SORT_MTIME = 4
};

#define DS_NO_NODE 0xFFFFFFFFu

// --- data ------------------------------------------------------------------

// The six numbers. No single one of them answers "how big is this".
typedef struct {
  uint64_t logical;          // apparent size; overstates sparse + iCloud
  uint64_t physical;         // allocated (du); counts every clone in full
  uint64_t exclusive;        // freed by deleting this object right now
  uint64_t shared_clones;    // of the remainder: shared with APFS clones
  uint64_t shared_snapshot;  // of the remainder: pinned by snapshot/seal
  uint64_t sparse_saving;    // holes never written
} DsSizes;

typedef struct {
  uint32_t node;
  uint32_t child_count;
  uint32_t unknown_below;  // >0 => this subtree's totals are a lower bound
  uint8_t kind;
  uint8_t state;
  uint8_t hardlink_alias;  // an extra link to an inode counted elsewhere
  uint8_t is_clone;
  int64_t mtime;
  DsSizes size;
} DsRow;

// One laid-out treemap block.
//
// area          = how big it is (physical)
// exclusive_frac= solid core: what deleting it would actually return
// snapshot_frac = of the rest, the part pinned by a snapshot rather than cloned
typedef struct {
  uint32_t node;
  float x, y, w, h;
  float exclusive_frac;
  float snapshot_frac;
  uint64_t physical;
  uint32_t aggregated;  // 0 = real node; N = stands for N tail items
  uint8_t kind;
  uint8_t state;
  uint8_t incomplete;  // total is a lower bound
  uint8_t container;   // children are nested inside: frame it, do not fill it
  uint8_t depth;       // 0 = direct child of the node being shown
  uint8_t _pad[3];
} DsRect;

// What deleting a *selection* would free. Never a sum — see ds_reclaim.
typedef struct {
  uint64_t bytes;
  uint64_t from_exclusive;
  uint64_t from_completed_families;
  uint64_t held_by_clones_outside;  // add those clones and this becomes bytes
  uint64_t pinned_by_snapshot;      // never freed by deleting files
  uint64_t physical;
  uint64_t files;
  uint64_t dirs;
  uint32_t families_completed;
  uint32_t unknown;
} DsReclaim;

typedef struct {
  DsSizes root;
  uint64_t dirs, files, symlinks;
  uint64_t hardlink_aliases, dataless, compressed, sparse, clone_members;
  uint64_t denied_tcc, denied_perm, errors;
  uint64_t volume_used, volume_total;
  uint64_t clone_reclaimable;
  uint64_t node_count, arena_bytes;
  double elapsed_secs;
  uint32_t root_unknown_below;
  uint32_t clone_families_whole, clone_families_total;
  uint32_t skipped_mounts;
  uint8_t cancelled;
  uint8_t root_is_volume_root;
  uint8_t _pad[6];
} DsSummary;

typedef struct {
  uint64_t walked_physical;
  uint64_t volume_used;
  uint64_t volume_total;
  int64_t delta;  // walked - used; positive means clones were over-counted
} DsReconcile;

typedef struct DsScan DsScan;  // a finished, immutable scan
typedef struct DsJob DsJob;    // a scan in flight

// Both callbacks arrive on the scan thread, NOT the main thread.
typedef void (*DsProgressFn)(void *ctx, uint64_t dirs, uint64_t files, uint64_t physical);
typedef void (*DsDoneFn)(void *ctx, DsScan *scan, const char *err);

// --- Full Disk Access ------------------------------------------------------
// FDA can be detected but never requested; there is no API, only System
// Settings.

int32_t ds_full_disk_access(void);
const char *ds_fda_settings_url(void);
// Which .app the user must actually add (TCC judges the responsible process).
size_t ds_responsible_app_hint(char *buf, size_t cap);

// --- running a scan --------------------------------------------------------

// Returns NULL only if the path is unusable. `threads`=0 picks the default.
DsJob *ds_scan_begin(const char *path, uint32_t threads, DsProgressFn on_progress,
                     DsDoneFn on_done, void *ctx);
void ds_scan_cancel(DsJob *job);  // still delivers a partial result
void ds_job_free(DsJob *job);     // joins the thread; cancel first if running
void ds_scan_free(DsScan *scan);

// --- reading a finished scan ----------------------------------------------
// All string functions return the length the string NEEDS, and write at most
// `cap` bytes including the NUL. len >= cap means "call again with more".

uint32_t ds_root(void);
uint32_t ds_node_count(const DsScan *scan);
uint32_t ds_parent(const DsScan *scan, uint32_t node);
bool ds_row(const DsScan *scan, uint32_t node, DsRow *out);
size_t ds_name(const DsScan *scan, uint32_t node, char *buf, size_t cap);
size_t ds_path(const DsScan *scan, uint32_t node, char *buf, size_t cap);

// Fills up to `cap` rows; returns how many were written.
size_t ds_children(const DsScan *scan, uint32_t node, int32_t sort, bool descending,
                   size_t offset, DsRow *out, size_t cap);

// Recursive squarified layout of the subtree under `node` into w x h.
// `max_depth` bounds nesting (0 = direct children only, drawn solid). Blocks
// thinner than min_px fold into one trailing block with `aggregated` set, at
// their own level. `gap` is the MAXIMUM space left between sibling tiles; each
// group scales it down towards its own typical tile size, so a folder of many
// small files is not spaced like a folder of a few large ones. Without any
// gap, separation would only ever be a side effect of how deeply nested two
// neighbours happen to be.
//
// Rectangles come back in PAINT ORDER (pre-order): a container always precedes
// the children drawn on top of it, so the last rectangle containing a point is
// the deepest one under the cursor.
size_t ds_treemap(const DsScan *scan, uint32_t node, float w, float h, float min_px,
                  uint32_t max_depth, float gap, DsRect *out, size_t cap);

// --- the point of all this -------------------------------------------------

// What deleting every node in `nodes` (and their subtrees) would ACTUALLY
// free. Overlapping selections are counted once. This cannot be a sum:
// three clones of a 64 MiB file each report exclusive=0, and deleting any
// two of them frees nothing at all.
bool ds_reclaim(const DsScan *scan, const uint32_t *nodes, size_t count, DsReclaim *out);

bool ds_summary(const DsScan *scan, DsSummary *out);
bool ds_reconcile(const DsScan *scan, DsReconcile *out);
size_t ds_reconcile_text(const DsScan *scan, char *buf, size_t cap);
// "why is reclaimable so low" — empty unless snapshots dominate.
size_t ds_snapshot_hint(const DsScan *scan, char *buf, size_t cap);

size_t ds_skipped_count(const DsScan *scan);
bool ds_skipped(const DsScan *scan, size_t index, char *path, size_t path_cap,
                char *reason, size_t reason_cap);

size_t ds_human(uint64_t bytes, char *buf, size_t cap);

// --- ABI guards ------------------------------------------------------------
// If any of these fire, src/ffi.rs and this header have drifted apart.

_Static_assert(sizeof(DsSizes) == 48, "DsSizes layout drift");
_Static_assert(sizeof(DsRow) == 72, "DsRow layout drift");
_Static_assert(sizeof(DsRect) == 56, "DsRect layout drift");
_Static_assert(sizeof(DsReclaim) == 72, "DsReclaim layout drift");
_Static_assert(sizeof(DsSummary) == 208, "DsSummary layout drift");
_Static_assert(sizeof(DsReconcile) == 32, "DsReconcile layout drift");

#ifdef __cplusplus
}
#endif
#endif  // DISK_SCANNER_H
