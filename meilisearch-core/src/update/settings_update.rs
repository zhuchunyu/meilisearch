use std::collections::{BTreeMap, BTreeSet};

use heed::Result as ZResult;
use fst::{set::OpBuilder, SetBuilder};
use sdset::SetBuf;
use meilisearch_schema::Schema;

use crate::database::{MainT, UpdateT};
use crate::settings::{UpdateState, SettingsUpdate, RankingRule};
use crate::update::documents_addition::reindex_all_documents;
use crate::update::{next_update_id, Update};
use crate::{store, MResult, Error};

pub fn push_settings_update(
    writer: &mut heed::RwTxn<UpdateT>,
    updates_store: store::Updates,
    updates_results_store: store::UpdatesResults,
    settings: SettingsUpdate,
) -> ZResult<u64> {
    let last_update_id = next_update_id(writer, updates_store, updates_results_store)?;

    let update = Update::settings(settings);
    updates_store.put_update(writer, last_update_id, &update)?;

    Ok(last_update_id)
}

pub fn apply_settings_update(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    settings: SettingsUpdate,
) -> MResult<()> {
    let mut must_reindex = false;

    let mut schema = match index.main.schema(writer)? {
        Some(schema) => schema,
        None => {
            match settings.identifier.clone() {
                UpdateState::Update(id) => Schema::with_identifier(&id),
                _ => return Err(Error::MissingIdentifier)
            }
        }
    };

    match settings.ranking_rules {
        UpdateState::Update(v) => {
            let ranked_field: Vec<&str> = v.iter().filter_map(RankingRule::field).collect();
            schema.update_ranked(ranked_field)?;
            index.main.put_ranking_rules(writer, &v)?;
            must_reindex = true;
        },
        UpdateState::Clear => {
            let clear: Vec<&str> = Vec::new();
            schema.update_ranked(clear)?;
            index.main.delete_ranking_rules(writer)?;
            must_reindex = true;
        },
        UpdateState::Nothing => (),
    }

    match settings.ranking_distinct {
        UpdateState::Update(v) => {
            index.main.put_ranking_distinct(writer, &v)?;
        },
        UpdateState::Clear => {
            index.main.delete_ranking_distinct(writer)?;
        },
        UpdateState::Nothing => (),
    }

    match settings.accept_new_fields {
        UpdateState::Update(v) => {
            schema.set_accept_new_fields(v);
        },
        UpdateState::Clear => {
            schema.set_accept_new_fields(true);
        },
        UpdateState::Nothing => (),
    }

    match settings.searchable_attributes.clone() {
        UpdateState::Update(v) => {
            schema.update_indexed(v)?;
            must_reindex = true;
        },
        UpdateState::Clear => {
            let clear: Vec<&str> = Vec::new();
            schema.update_indexed(clear)?;
            must_reindex = true;
        },
        UpdateState::Nothing => (),
    }
    match settings.displayed_attributes.clone() {
        UpdateState::Update(v) => schema.update_displayed(v)?,
        UpdateState::Clear => {
            let clear: Vec<&str> = Vec::new();
            schema.update_displayed(clear)?;
        },
        UpdateState::Nothing => (),
    }

    index.main.put_schema(writer, &schema)?;

    match settings.stop_words {
        UpdateState::Update(stop_words) => {
            if apply_stop_words_update(writer, index, stop_words)? {
                must_reindex = true;
            }
        },
        UpdateState::Clear => {
            if apply_stop_words_update(writer, index, BTreeSet::new())? {
                must_reindex = true;
            }
        },
        UpdateState::Nothing => (),
    }

    match settings.synonyms {
        UpdateState::Update(synonyms) => apply_synonyms_update(writer, index, synonyms)?,
        UpdateState::Clear => apply_synonyms_update(writer, index, BTreeMap::new())?,
        UpdateState::Nothing => (),
    }

    if must_reindex {
        reindex_all_documents(writer, index)?;
    }

    Ok(())
}

pub fn apply_stop_words_update(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    stop_words: BTreeSet<String>,
) -> MResult<bool> {

    let old_stop_words: BTreeSet<String> = index.main
        .stop_words_fst(writer)?
        .unwrap_or_default()
        .stream()
        .into_strs().unwrap().into_iter().collect();

    let deletion: BTreeSet<String> = old_stop_words.difference(&stop_words).cloned().collect();
    let addition: BTreeSet<String> = stop_words.difference(&old_stop_words).cloned().collect();

    if !addition.is_empty() {
        apply_stop_words_addition(
            writer,
            index,
            addition
        )?;
    }

    if !deletion.is_empty() {
        apply_stop_words_deletion(
            writer,
            index,
            deletion
        )?;
        return Ok(true)
    }

    let stop_words_fst = fst::Set::from_iter(stop_words)?;
    index.main.put_words_fst(writer, &stop_words_fst)?;
    Ok(false)
}

fn apply_stop_words_addition(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    addition: BTreeSet<String>,
) -> MResult<()> {

    let main_store = index.main;
    let postings_lists_store = index.postings_lists;

    let mut stop_words_builder = SetBuilder::memory();

    for word in addition {
        stop_words_builder.insert(&word).unwrap();
        // we remove every posting list associated to a new stop word
        postings_lists_store.del_postings_list(writer, word.as_bytes())?;
    }

    // create the new delta stop words fst
    let delta_stop_words = stop_words_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    // we also need to remove all the stop words from the main fst
    if let Some(word_fst) = main_store.words_fst(writer)? {
        let op = OpBuilder::new()
            .add(&word_fst)
            .add(&delta_stop_words)
            .difference();

        let mut word_fst_builder = SetBuilder::memory();
        word_fst_builder.extend_stream(op).unwrap();
        let word_fst = word_fst_builder
            .into_inner()
            .and_then(fst::Set::from_bytes)
            .unwrap();

        main_store.put_words_fst(writer, &word_fst)?;
    }

    // now we add all of these stop words from the main store
    let stop_words_fst = main_store.stop_words_fst(writer)?.unwrap_or_default();

    let op = OpBuilder::new()
        .add(&stop_words_fst)
        .add(&delta_stop_words)
        .r#union();

    let mut stop_words_builder = SetBuilder::memory();
    stop_words_builder.extend_stream(op).unwrap();
    let stop_words_fst = stop_words_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    main_store.put_stop_words_fst(writer, &stop_words_fst)?;

    Ok(())
}

fn apply_stop_words_deletion(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    deletion: BTreeSet<String>,
) -> MResult<()> {

    let mut stop_words_builder = SetBuilder::memory();

    for word in deletion {
        stop_words_builder.insert(&word).unwrap();
    }

    // create the new delta stop words fst
    let delta_stop_words = stop_words_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    // now we delete all of these stop words from the main store
    let stop_words_fst = index.main.stop_words_fst(writer)?.unwrap_or_default();

    let op = OpBuilder::new()
        .add(&stop_words_fst)
        .add(&delta_stop_words)
        .difference();

    let mut stop_words_builder = SetBuilder::memory();
    stop_words_builder.extend_stream(op).unwrap();
    let stop_words_fst = stop_words_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    Ok(index.main.put_stop_words_fst(writer, &stop_words_fst)?)
}

pub fn apply_synonyms_update(
    writer: &mut heed::RwTxn<MainT>,
    index: &store::Index,
    synonyms: BTreeMap<String, Vec<String>>,
) -> MResult<()> {

    let main_store = index.main;
    let synonyms_store = index.synonyms;

    let mut synonyms_builder = SetBuilder::memory();
    synonyms_store.clear(writer)?;
    for (word, alternatives) in synonyms.clone() {
        synonyms_builder.insert(&word).unwrap();

        let alternatives = {
            let alternatives = SetBuf::from_dirty(alternatives);
            let mut alternatives_builder = SetBuilder::memory();
            alternatives_builder.extend_iter(alternatives).unwrap();
            let bytes = alternatives_builder.into_inner().unwrap();
            fst::Set::from_bytes(bytes).unwrap()
        };

        synonyms_store.put_synonyms(writer, word.as_bytes(), &alternatives)?;
    }

    let synonyms_set = synonyms_builder
        .into_inner()
        .and_then(fst::Set::from_bytes)
        .unwrap();

    main_store.put_synonyms_fst(writer, &synonyms_set)?;

    Ok(())
}
