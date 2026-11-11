#[cfg(test)]
pub(in crate::services::discord) use crate::domain::message_split::semantic_boundaries::message_split_boundary;
pub(in crate::services::discord) use crate::domain::message_split::semantic_boundaries::{
    semantic_chunk_separator_needed, semantic_sentence_split_boundary,
    text_ends_inside_open_code_fence,
};
