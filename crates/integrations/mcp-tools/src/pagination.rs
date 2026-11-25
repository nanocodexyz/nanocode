use std::{collections::HashSet, future::Future, time::Duration};

const MCP_CATALOG_PAGES_MAX: usize = 100;
const MCP_CATALOG_ITEMS_MAX: usize = 2_048;
const MCP_PAGINATION_CURSOR_BYTES_MAX: usize = 64 * 1024;
const DEFAULT_MCP_PAGINATION_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn collect_paginated<T, F, Fut>(
    method: &str,
    mut fetch: F,
) -> Result<Vec<T>, String>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<String>), String>>,
{
    let collect = async {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();

        for _ in 0..MCP_CATALOG_PAGES_MAX {
            let (items, next_cursor) = fetch(cursor.clone()).await?;
            if items.len() > MCP_CATALOG_ITEMS_MAX.saturating_sub(collected.len()) {
                return Err(format!(
                    "{method} exceeded the catalog limit of {MCP_CATALOG_ITEMS_MAX} items"
                ));
            }
            collected.extend(items);
            assert!(collected.len() <= MCP_CATALOG_ITEMS_MAX);

            let Some(next_cursor) = next_cursor else {
                return Ok(collected);
            };
            if next_cursor.len() > MCP_PAGINATION_CURSOR_BYTES_MAX {
                return Err(format!(
                    "{method} returned a pagination cursor exceeding {MCP_PAGINATION_CURSOR_BYTES_MAX} bytes"
                ));
            }
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(format!("{method} returned a repeated pagination cursor"));
            }
            assert!(seen_cursors.len() <= MCP_CATALOG_PAGES_MAX);
            cursor = Some(next_cursor);
        }

        Err(format!(
            "{method} exceeded the pagination limit of {MCP_CATALOG_PAGES_MAX} pages"
        ))
    };

    let timeout = DEFAULT_MCP_PAGINATION_TIMEOUT;
    nanocode_platform::timeout(timeout, collect)
        .await
        .map_err(|_| format!("{method} pagination timed out after {timeout:?}"))?
}
