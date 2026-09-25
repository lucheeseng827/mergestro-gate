# turnover-history

The git side, in pure Rust via `gix`: list the commits to classify (with origin attribution
from the author and message), load both sides of every changed blob with rename and copy
detection, drop what should never be counted (vendored paths, binaries, oversized files,
unknown languages), and drive `turnover-core` over every commit in parallel with rayon. Churn
attribution runs afterwards as one ordered pass, carrying the still-open additions across
incremental refreshes.

Merge commits are skipped by default: their first-parent diff would re-attribute a whole
branch to one commit and one author.

See the module [ARCHITECTURE](../../ARCHITECTURE.md).

## Architecture

```mermaid
flowchart TD
    open["open(path)\nThreadSafeRepository"] --> list["list_commits\ntips, hidden, since, skip_merges"]
    list --> metas["CommitMeta[]\nsha, time, author, Origin"]
    metas --> par{"rayon par_iter\none thread-local repo each"}
    par --> diff["diff parent → commit\nrename + copy detection"]
    diff --> filter["drop vendored paths,\nbinaries, oversized, unknown langs"]
    filter --> mask["turnover-lang\nmask per blob"]
    mask --> classify["turnover-core::classify"]
    classify --> rows["CommitSignals[]"]
    rows --> churn["attribute_churn\none ordered pass"]
    pending["pending additions\nfrom the last refresh"] --> churn
    churn --> out["rows + still-open additions"]
```

## Call flow

```mermaid
sequenceDiagram
    participant C as caller (gate / CLI)
    participant H as turnover-history
    participant G as gix
    participant L as turnover-lang
    C->>H: open(path)
    C->>H: list_commits(repo, opts)
    H->>G: revision walk (sorted by commit time)
    G-->>H: commit metadata
    H-->>C: Vec<CommitMeta> (newest first)
    C->>H: classify_all(repo, metas, opts, cfg, progress)
    loop every commit, in parallel
        H->>G: diff against the parent, with rename detection
        H->>L: mask each changed blob
        H->>H: turnover-core::classify
    end
    H-->>C: (Vec<CommitSignals>, Stats)
    C->>H: attribute_churn(pending, rows, seeds, horizon)
    H-->>C: rows with churn charged, additions still open
```

## Quickstart

```rust
use turnover_core::SignalConfig;
use turnover_history::{classify_all, list_commits, open, Error, WalkOptions};

fn walk_head() -> Result<(), Error> {
    let repo = open(".")?;
    let opts = WalkOptions {
        tips: vec!["HEAD".to_string()],
        ..Default::default()
    };
    let metas = list_commits(&repo, &opts)?;
    let (rows, stats) = classify_all(&repo, &metas, &opts, &SignalConfig::default(), &|_done| {})?;

    println!(
        "{} commits, {} rows, {} lines parsed",
        metas.len(),
        rows.len(),
        stats.lines_parsed
    );
    Ok(())
}
```

`cargo test -p turnover-history` builds a synthetic repository with the `git` CLI and checks
the buckets against the unit model, so the walk and the classifier cannot drift apart.
