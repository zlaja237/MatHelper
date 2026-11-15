//! Inköpslistan: en delad lista för hushållet (sparad i
//! `data/shopping_list.json`) plus förslag räknade ur de kvitton som redan
//! är synkade — "du brukar köpa X ungefär var N:e dag, och det var M dagar
//! sedan sist".

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::Json;
use kvitto_core::categorize::normalise;
use kvitto_core::{LineKind, Rules};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::serve_sync::AppState;

const LIST_PATH: &str = "data/shopping_list.json";
const PAGE: &str = include_str!("shopping.html");

/// Läs-ändra-skriv av listfilen sker under det här låset, så två telefoner
/// som bockar av samtidigt inte skriver över varandra.
static LIST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
pub struct ListItem {
    id: u64,
    name: String,
    category: String,
    done: bool,
    added_unix: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ShoppingList {
    items: Vec<ListItem>,
    next_id: u64,
}

fn read_list() -> ShoppingList {
    std::fs::read_to_string(LIST_PATH)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_list(list: &ShoppingList) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(LIST_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(list).map_err(std::io::Error::other)?;
    // Skriv till en temporär fil och byt namn, så en krasch mitt i aldrig
    // lämnar en halv lista efter sig.
    let tmp = format!("{LIST_PATH}.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, LIST_PATH)
}

fn modify_list(f: impl FnOnce(&mut ShoppingList)) -> Result<Json<ShoppingList>, StatusCode> {
    let _guard = LIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = read_list();
    f(&mut list);
    write_list(&list).map_err(|e| {
        tracing::warn!("could not save shopping list: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(list))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Statistik ur kvittona
// ---------------------------------------------------------------------------

/// Allt vi vet om en vara (sammanslagen på normaliserat namn).
struct ProductStats {
    name: String,
    category: Option<String>,
    /// Unika inköpsdagar, sorterade.
    days: Vec<chrono::NaiveDate>,
}

fn product_stats(state: &AppState) -> HashMap<String, ProductStats> {
    let receipts = match state.store.merged(&state.profiles) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("could not read receipts for suggestions: {e}");
            return HashMap::new();
        }
    };
    let mut stats: HashMap<String, ProductStats> = HashMap::new();
    // `merged` är sorterad på köptid, så senaste namn/kategori vinner.
    for r in &receipts {
        let day = r.date();
        for line in &r.lines {
            if !matches!(line.kind, LineKind::Item) || line.amount.kr() <= 0.0 {
                continue;
            }
            let key = normalise(&line.description);
            if key.is_empty() {
                continue;
            }
            let entry = stats.entry(key).or_insert_with(|| ProductStats {
                name: line.description.clone(),
                category: None,
                days: Vec::new(),
            });
            entry.name = line.description.clone();
            if line.category.is_some() {
                entry.category = line.category.clone();
            }
            if entry.days.last() != Some(&day) {
                entry.days.push(day);
            }
        }
    }
    stats
}

fn median(mut v: Vec<i64>) -> f64 {
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0
    }
}

#[derive(Serialize)]
pub struct Suggestion {
    name: String,
    category: String,
    /// Antal dagar varan köpts.
    times: usize,
    /// Typiskt antal dagar mellan köpen.
    every_days: f64,
    days_since: i64,
}

#[derive(Serialize)]
pub struct KnownItem {
    name: String,
    category: String,
}

#[derive(Serialize)]
pub struct SuggestionsResponse {
    suggestions: Vec<Suggestion>,
    /// Varor som köpts förut, vanligast först — till autokompletteringen.
    known: Vec<KnownItem>,
}

const MIN_PURCHASES: usize = 3;
const MAX_SUGGESTIONS: usize = 30;
const MAX_KNOWN: usize = 400;

fn build_suggestions(state: &AppState) -> SuggestionsResponse {
    let today = chrono::Local::now().date_naive();
    let stats = product_stats(state);

    let on_list: Vec<String> = read_list()
        .items
        .iter()
        .filter(|i| !i.done)
        .map(|i| normalise(&i.name))
        .collect();

    let mut scored: Vec<(f64, Suggestion)> = Vec::new();
    for (key, s) in &stats {
        if s.days.len() < MIN_PURCHASES || on_list.contains(key) {
            continue;
        }
        let gaps: Vec<i64> = s.days.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
        let every = median(gaps).max(1.0);
        let last = *s.days.last().expect("MIN_PURCHASES > 0");
        let days_since = (today - last).num_days().max(0);
        let ratio = days_since as f64 / every;
        // Inte dags än, eller så länge sedan att vanan verkar ha upphört.
        if ratio < 0.8 || ratio > 3.0 {
            continue;
        }
        // Ofta köpta varor först, bland dem som är "dags".
        let score = ratio.min(1.5) * ((s.days.len() + 1) as f64).ln();
        scored.push((
            score,
            Suggestion {
                name: s.name.clone(),
                category: s.category.clone().unwrap_or_else(|| "Övrigt".into()),
                times: s.days.len(),
                every_days: every,
                days_since,
            },
        ));
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let suggestions = scored.into_iter().take(MAX_SUGGESTIONS).map(|(_, s)| s).collect();

    let mut known: Vec<&ProductStats> = stats.values().collect();
    known.sort_by(|a, b| b.days.len().cmp(&a.days.len()).then_with(|| a.name.cmp(&b.name)));
    let known = known
        .into_iter()
        .take(MAX_KNOWN)
        .map(|s| KnownItem {
            name: s.name.clone(),
            category: s.category.clone().unwrap_or_else(|| "Övrigt".into()),
        })
        .collect();

    SuggestionsResponse { suggestions, known }
}

/// Kategori för en vara som skrivs in för hand: samma som förra gången den
/// köptes, annars kategorireglerna, annars "Övrigt".
fn category_for_name(state: &AppState, name: &str) -> String {
    let key = normalise(name);
    if let Some(c) = product_stats(state).get(&key).and_then(|s| s.category.clone()) {
        return c;
    }
    let raw = std::fs::read_to_string(crate::CATEGORIES_PATH).unwrap_or_default();
    let rules: Rules = toml::from_str(&raw).unwrap_or_default();
    for (cat, subs) in rules.categories.iter() {
        if subs.iter().any(|s| key.contains(&normalise(s))) {
            return cat.clone();
        }
    }
    "Övrigt".into()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}

pub async fn get_list() -> Json<ShoppingList> {
    Json(read_list())
}

#[derive(Deserialize)]
pub struct AddRequest {
    name: String,
    category: Option<String>,
}

pub async fn add_item(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddRequest>,
) -> Result<Json<ShoppingList>, StatusCode> {
    let name = body.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let category = match body.category.map(|c| c.trim().to_string()) {
        Some(c) if !c.is_empty() => c,
        _ => category_for_name(&state, &name),
    };
    modify_list(|list| {
        // Står varan redan på listan (ej avbockad) lägger vi inte till den igen.
        let key = normalise(&name);
        if list.items.iter().any(|i| !i.done && normalise(&i.name) == key) {
            return;
        }
        list.next_id += 1;
        list.items.push(ListItem {
            id: list.next_id,
            name,
            category,
            done: false,
            added_unix: now_unix(),
        });
    })
}

#[derive(Deserialize)]
pub struct IdRequest {
    id: u64,
}

pub async fn toggle_item(Json(body): Json<IdRequest>) -> Result<Json<ShoppingList>, StatusCode> {
    modify_list(|list| {
        if let Some(item) = list.items.iter_mut().find(|i| i.id == body.id) {
            item.done = !item.done;
        }
    })
}

pub async fn remove_item(Json(body): Json<IdRequest>) -> Result<Json<ShoppingList>, StatusCode> {
    modify_list(|list| list.items.retain(|i| i.id != body.id))
}

pub async fn clear_done() -> Result<Json<ShoppingList>, StatusCode> {
    modify_list(|list| list.items.retain(|i| !i.done))
}

pub async fn get_suggestions(State(state): State<Arc<AppState>>) -> Json<SuggestionsResponse> {
    Json(build_suggestions(&state))
}

// ---------------------------------------------------------------------------
// För iOS-genvägen ("Synka inköp"): enkla former som Genvägar läser direkt.
// ---------------------------------------------------------------------------

/// Namnen på alla varor som inte är avbockade — en ren JSON-array av strängar.
pub async fn open_names() -> Json<Vec<String>> {
    Json(read_list().items.into_iter().filter(|i| !i.done).map(|i| i.name).collect())
}

#[derive(Deserialize)]
pub struct NamesRequest {
    names: Vec<String>,
}

/// Bocka av varor på namn (det Påminnelser vet om), skiftlägesokänsligt.
pub async fn done_by_name(Json(body): Json<NamesRequest>) -> Result<Json<ShoppingList>, StatusCode> {
    let keys: Vec<String> = body.names.iter().map(|n| normalise(n)).collect();
    modify_list(|list| {
        for item in list.items.iter_mut() {
            if !item.done && keys.contains(&normalise(&item.name)) {
                item.done = true;
            }
        }
    })
}

/// BankID-sidan som MCP-verktyget länkar till: /login?source=ica&profile=du.
pub async fn login_page() -> Html<&'static str> {
    Html(include_str!("login.html"))
}

/// Alla synkade kvitton (alla profiler, dubbletter sammanslagna), äldst
/// först. Belopp i öre. Läses av MCP-servern för frågor om inköpen.
pub async fn get_receipts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<kvitto_core::Receipt>>, StatusCode> {
    state.store.merged(&state.profiles).map(Json).map_err(|e| {
        tracing::warn!("could not read receipts: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// Rena fakta per vara, utan urval eller rangordning — för Claude via MCP,
/// som själv avgör vad som är "dags" utifrån sammanhanget.
#[derive(Serialize)]
pub struct Pattern {
    name: String,
    category: String,
    /// Antal unika dagar varan köpts.
    times: usize,
    first_bought: String,
    last_bought: String,
    days_since: i64,
    /// Median antal dagar mellan köpen (saknas om köpt bara en dag).
    every_days: Option<f64>,
    /// Kortaste och längsta uppehåll mellan köp.
    min_gap_days: Option<i64>,
    max_gap_days: Option<i64>,
    /// På inköpslistan just nu (ej avbockad).
    on_list: bool,
}

pub async fn get_patterns(State(state): State<Arc<AppState>>) -> Json<Vec<Pattern>> {
    let today = chrono::Local::now().date_naive();
    let on_list: Vec<String> =
        read_list().items.iter().filter(|i| !i.done).map(|i| normalise(&i.name)).collect();
    let mut out: Vec<Pattern> = product_stats(&state)
        .into_iter()
        .filter_map(|(key, s)| {
            let first = *s.days.first()?;
            let last = *s.days.last()?;
            let gaps: Vec<i64> = s.days.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
            Some(Pattern {
                category: s.category.clone().unwrap_or_else(|| "Övrigt".into()),
                times: s.days.len(),
                first_bought: first.to_string(),
                last_bought: last.to_string(),
                days_since: (today - last).num_days().max(0),
                every_days: if gaps.is_empty() { None } else { Some(median(gaps.clone())) },
                min_gap_days: gaps.iter().min().copied(),
                max_gap_days: gaps.iter().max().copied(),
                on_list: on_list.contains(&key),
                name: s.name,
            })
        })
        .collect();
    out.sort_by(|a, b| b.times.cmp(&a.times).then_with(|| a.name.cmp(&b.name)));
    Json(out)
}
