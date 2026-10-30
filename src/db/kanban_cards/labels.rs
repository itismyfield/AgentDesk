use sqlx::PgExecutor;

/// How [`set_card_labels_pg`] changes `metadata.labels`, the comma-joined label string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardLabelsUpdate<'a> {
    /// Replace the labels; an empty string clears them.
    Replace(&'a str),
    /// Add one label unless it is already present.
    Append(&'a str),
}

/// Joins label names into the stored `metadata.labels` form, dropping blank names.
pub fn card_labels_csv<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    names
        .into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

// One UPDATE re-evaluates on the latest row version, so concurrent writers of other keys survive.
// Non-object metadata (array, string, NULL) carries no readable keys and is replaced by `{}` first.
const SET_LABELS_SQL: &str = "UPDATE kanban_cards
     SET metadata = jsonb_set(
             CASE WHEN jsonb_typeof(metadata) = 'object' THEN metadata ELSE '{}'::jsonb END,
             '{labels}',
             to_jsonb(CAST($2 AS text))),
         updated_at = NOW()
     WHERE id = $1";

const APPEND_LABEL_SQL: &str = "UPDATE kanban_cards
     SET metadata = jsonb_set(
             CASE WHEN jsonb_typeof(metadata) = 'object' THEN metadata ELSE '{}'::jsonb END,
             '{labels}',
             to_jsonb((
                 SELECT array_to_string(
                     CASE WHEN CAST($2 AS text) = ANY(existing.labels) THEN existing.labels
                          ELSE array_append(existing.labels, CAST($2 AS text)) END,
                     ',')
                 FROM (SELECT ARRAY(
                     SELECT BTRIM(item)
                     FROM unnest(string_to_array(
                         CASE WHEN jsonb_typeof(metadata -> 'labels') = 'string'
                              THEN metadata ->> 'labels' ELSE '' END,
                         ',')) AS item
                     WHERE BTRIM(item) <> '') AS labels) AS existing))),
         updated_at = NOW()
     WHERE id = $1";

/// Writes `kanban_cards.metadata.labels` for existing cards; other metadata keys are left untouched.
/// Issue sync, `card create-from-issue`, and card markers all go through here. Returns whether the card exists.
pub async fn set_card_labels_pg<'e, E: PgExecutor<'e>>(
    executor: E,
    card_id: &str,
    update: CardLabelsUpdate<'_>,
) -> Result<bool, String> {
    let (sql, value) = match update {
        CardLabelsUpdate::Replace(labels) => (SET_LABELS_SQL, labels),
        CardLabelsUpdate::Append(label) if !label.trim().is_empty() => {
            (APPEND_LABEL_SQL, label.trim())
        }
        CardLabelsUpdate::Append(_) => {
            return Err(format!("set labels for card {card_id}: blank label"));
        }
    };
    let updated = sqlx::query(sql)
        .bind(card_id)
        .bind(value)
        .execute(executor)
        .await
        .map_err(|error| format!("set labels for card {card_id}: {error}"))?
        .rows_affected();
    Ok(updated > 0)
}
