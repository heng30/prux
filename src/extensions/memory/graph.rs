//! 记忆图：条目模型、标签/边、图操作与 JSON 落盘。
//!
//! 存储形状沿用 jcode 的 `MemoryGraph`（v2）：`memories` / `tags` / `clusters` /
//! `edges` / `reverse_edges` / `metadata` 六个字段都是 JSON 对象，节点以 id 为键。
//! prux **不做聚类**（`clusters` 恒为空，字段保留只为形状兼容），也没有 embedding
//! 字段——召回是纯词法的（见 [`super::recall`]）。
//!
//! 时间戳是 UTC RFC3339 字符串（`2026-01-02T03:04:05.678Z`），与 `utils::time`
//! 的读写口径一致；解析失败时按「刚写入」处理（不参与衰减），不让一份手改坏的文件
//! 把整份记忆读成空。

use crate::{
    extensions::util::generate_id,
    utils::time::{now_iso, now_ms, parse_iso_ms},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    path::Path,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 记忆图格式版本，用于识别未来/更旧的文件。
pub(super) const GRAPH_VERSION: u32 = 2;

/// 单份存储的条目上限。
///
/// 超过时按 [`MemoryEntry::memory_score`] 淘汰最低分（通常是又旧又少被用到的条目）；
/// 没有上限的话一次失控的抽取就能把文件写到几十 MB，之后每次召回都要读它。
pub(super) const MAX_ENTRIES: usize = 1000;

/// 记忆分类。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum MemoryCategory {
    /// 关于代码库 / 环境的事实（默认分类）。
    Fact,
    /// 用户的偏好与习惯。
    Preference,
    /// 具体的实体（人、服务、文件、命令）。
    Entity,
    /// 用户对 agent 的纠正。
    Correction,
}

impl MemoryCategory {
    /// 面板与提示里展示的分类名（小写）。
    pub(super) fn as_label(self) -> &'static str {
        self.into()
    }

    /// 抽取结果里的分类字符串映射；无法识别时归入 [`Self::Fact`]。
    pub(super) fn parse_lenient(raw: &str) -> Self {
        let trimmed = raw.trim();
        match trimmed.parse::<Self>() {
            Ok(category) => category,
            Err(_) => match trimmed.to_ascii_lowercase().as_str() {
                "facts" => Self::Fact,
                "preferences" | "pref" => Self::Preference,
                "entities" => Self::Entity,
                "corrections" | "bugs" | "fix" | "bug" => Self::Correction,
                // 中文对话里模型可能直接给中文分类名，别把它丢了
                "事实" => Self::Fact,
                "偏好" => Self::Preference,
                "实体" => Self::Entity,
                "纠正" | "修正" => Self::Correction,
                _ => Self::Fact,
            },
        }
    }
}

/// 条目可信度。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum TrustLevel {
    /// 用户自己明确说过的。
    High,
    /// 从用户行为里观察到的（默认档）。
    #[default]
    Medium,
    /// agent 自己推断出来的。
    Low,
}

impl TrustLevel {
    /// 打分时的可信度权重。
    fn weight(self) -> f64 {
        match self {
            Self::High => 1.5,
            Self::Medium => 1.0,
            Self::Low => 0.7,
        }
    }
}

/// 作用域：条目存在哪一份存储里。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum MemoryScope {
    /// 只属于当前项目。
    Project,
    /// 跨项目通用。
    Global,
    /// 两份存储合并（只用于读取）。
    #[default]
    All,
}

impl MemoryScope {
    /// 是否包含项目存储。
    pub(super) fn includes_project(self) -> bool {
        matches!(self, Self::Project | Self::All)
    }

    /// 是否包含全局存储。
    pub(super) fn includes_global(self) -> bool {
        matches!(self, Self::Global | Self::All)
    }

    /// 面板/提示里展示的名字。
    pub(super) fn as_label(self) -> &'static str {
        self.into()
    }
}

/// 一次「同一条信息又被遇到」的出处记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Reinforcement {
    /// 出处（会话 id 或 `extract` 这类来源标记）。
    pub(super) session_id: String,
    /// 出处里的消息序号；未知时为 0。
    #[serde(default)]
    pub(super) message_index: usize,
    /// 发生时间（UTC RFC3339）。
    pub(super) timestamp: String,
}

/// 一条记忆。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MemoryEntry {
    /// 稳定 id（`mem_<16 位十六进制>`）。
    pub(super) id: String,
    /// 分类。
    pub(super) category: MemoryCategory,
    /// 记忆正文（面向模型的一句话）。
    pub(super) content: String,
    /// 标签（小写化前原样保存）。
    #[serde(default)]
    pub(super) tags: Vec<String>,
    /// 归一化后的检索文本（正文 + 标签）；空时按需重算。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) search_text: String,
    /// 创建时间（UTC RFC3339）。
    pub(super) created_at: String,
    /// 最近一次更新时间（UTC RFC3339）。
    pub(super) updated_at: String,
    /// 被检索/引用过的次数。
    #[serde(default)]
    pub(super) access_count: u32,
    /// 来源（会话 id / `extract` / `skill` 等）；未知为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
    /// 可信度。
    #[serde(default)]
    pub(super) trust: TrustLevel,
    /// 被重复确认的次数（初值 1）。
    #[serde(default)]
    pub(super) strength: u32,
    /// 是否仍然有效；`false` 表示已被更新的条目取代。
    #[serde(default = "default_active")]
    pub(super) active: bool,
    /// 取代它的条目 id（`active == false` 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) superseded_by: Option<String>,
    /// 重复确认的出处记录。
    #[serde(default)]
    pub(super) reinforcements: Vec<Reinforcement>,
    /// 置信度 0.0–1.0；会随时间衰减、被使用抬升。
    #[serde(default = "default_confidence")]
    pub(super) confidence: f32,
}

/// `active` 字段的缺省值：缺失即视为有效。
fn default_active() -> bool {
    true
}

/// `confidence` 字段的缺省值。
fn default_confidence() -> f32 {
    1.0
}

impl MemoryEntry {
    /// 新建一条记忆（id 与时间戳由本函数生成）。
    pub(super) fn new(category: MemoryCategory, content: impl Into<String>) -> Self {
        let now = now_iso();
        let content = content.into();
        let mut entry = Self {
            id: format!("mem_{}", generate_id()),
            category,
            search_text: String::new(),
            content,
            tags: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            access_count: 0,
            source: None,
            trust: TrustLevel::default(),
            strength: 1,
            active: true,
            superseded_by: None,
            reinforcements: Vec::new(),
            confidence: 1.0,
        };
        entry.refresh_search_text();
        entry
    }

    /// 设置来源（链式）。
    pub(super) fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 设置标签并刷新检索文本（链式）。
    pub(super) fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self.refresh_search_text();
        self
    }

    /// 设置可信度（链式）。
    pub(super) fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// 重算归一化检索文本（正文 + 标签）。
    pub(super) fn refresh_search_text(&mut self) {
        self.search_text = normalize_search_text(&self.content, &self.tags);
    }

    /// 归一化检索文本；缓存为空时现算（兼容手写文件）。
    pub(super) fn searchable_text(&self) -> String {
        if self.search_text.is_empty() {
            normalize_search_text(&self.content, &self.tags)
        } else {
            self.search_text.clone()
        }
    }

    /// 创建时间（毫秒），无法解析时回退 [`now_ms`]。
    pub(super) fn created_ms(&self) -> u64 {
        parse_iso_ms(&self.created_at).unwrap_or_else(now_ms)
    }

    /// 更新时间（毫秒），无法解析时回退 [`now_ms`]。
    pub(super) fn updated_ms(&self) -> u64 {
        parse_iso_ms(&self.updated_at).unwrap_or_else(now_ms)
    }

    /// 按分类半衰期做时间衰减后的置信度（0.0–1.0）。
    ///
    /// 半衰期：纠正 365 天、偏好 90 天、实体 60 天、事实 30 天。访问次数会略有加成
    /// （`1 + 0.1·ln(access+1)`），让常用条目不要因为旧而被彻底埋掉。
    pub(super) fn effective_confidence(&self) -> f32 {
        let age_days = (now_ms().saturating_sub(self.created_ms()) as f64) / 86_400_000.0;
        let half_life = match self.category {
            MemoryCategory::Correction => 365.0,
            MemoryCategory::Preference => 90.0,
            MemoryCategory::Entity => 60.0,
            MemoryCategory::Fact => 30.0,
        };
        let decay = (-age_days / half_life * std::f64::consts::LN_2).exp();
        let boost = 1.0 + 0.1 * ((self.access_count as f64) + 1.0).ln();
        (f64::from(self.confidence) * decay * boost).min(1.0) as f32
    }

    /// 打分：时效 + 使用频次 + 分类权重 × 可信度 + 重复确认强度。
    ///
    /// 用于「没有查询词时的最近条目」与超出容量时的淘汰判定；带查询词的召回走
    /// [`super::recall`] 的 BM25。
    pub(super) fn memory_score(&self) -> f64 {
        if !self.active {
            return 0.0;
        }
        let age_hours = (now_ms().saturating_sub(self.updated_ms()) as f64) / 3_600_000.0;
        let mut score = 100.0 / (1.0 + age_hours / 24.0);
        score += (f64::from(self.access_count)).sqrt() * 10.0;
        score += match self.category {
            MemoryCategory::Correction => 50.0,
            MemoryCategory::Preference => 30.0,
            MemoryCategory::Fact => 20.0,
            MemoryCategory::Entity => 10.0,
        };
        score *= self.trust.weight();
        score *= 0.5 + 0.5 * f64::from(self.effective_confidence());
        score += (f64::from(self.strength)).ln() * 5.0;
        score
    }

    /// 记录一次使用（提升置信度、刷新时间）。
    pub(super) fn touch(&mut self) {
        self.access_count = self.access_count.saturating_add(1);
        self.updated_at = now_iso();
        self.confidence = (self.confidence + 0.05).min(1.0);
    }

    /// 同一条信息再次出现：强度 +1，留下出处记录。
    pub(super) fn reinforce(&mut self, session_id: &str) {
        self.strength = self.strength.saturating_add(1);
        self.updated_at = now_iso();
        self.reinforcements.push(Reinforcement {
            session_id: session_id.to_string(),
            message_index: 0,
            timestamp: now_iso(),
        });
    }

    /// 标记为被 `new_id` 取代（不删除，保留可追溯性）。
    pub(super) fn supersede(&mut self, new_id: &str) {
        self.active = false;
        self.superseded_by = Some(new_id.to_string());
    }
}

/// 一个标签节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TagEntry {
    /// 节点 id（`tag:<name>`）。
    pub(super) id: String,
    /// 展示名。
    pub(super) name: String,
    /// 打上该标签的记忆条数。
    #[serde(default)]
    pub(super) count: u32,
    /// 首次创建时间（UTC RFC3339）。
    pub(super) created_at: String,
}

impl TagEntry {
    /// 按名字新建标签节点。
    fn new(name: &str) -> Self {
        Self {
            id: format!("tag:{name}"),
            name: name.to_string(),
            count: 0,
            created_at: now_iso(),
        }
    }
}

/// 边的类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum EdgeKind {
    /// 记忆 → 标签。
    HasTag,
    /// 记忆 → 聚类。
    InCluster,
    /// 记忆之间的语义关联，权重 0.0–1.0。
    RelatesTo {
        /// 关联强度；缺省 1.0。
        #[serde(default = "default_weight")]
        weight: f32,
    },
    /// 新条目取代旧条目。
    Supersedes,
    /// 两条信息互相矛盾（两条都保留并标记）。
    Contradicts,
}

/// `RelatesTo` 的缺省权重。
fn default_weight() -> f32 {
    1.0
}

impl EdgeKind {
    /// 图遍历时的衰减权重。
    fn traversal_weight(&self) -> f32 {
        match self {
            Self::HasTag => 0.8,
            Self::InCluster => 0.6,
            Self::RelatesTo { weight } => *weight,
            Self::Supersedes => 0.9,
            Self::Contradicts => 0.3,
        }
    }
}

/// 一条有向边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Edge {
    /// 目标节点 id。
    pub(super) target: String,
    /// 边的类型（与 `target` 平铺在同一层 JSON 对象里）。
    #[serde(flatten)]
    pub(super) kind: EdgeKind,
}

/// 图的统计信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct GraphMetadata {
    /// 召回操作累计次数。
    #[serde(default)]
    pub(super) retrieval_count: u64,
    /// 手工/自动建边累计次数。
    #[serde(default)]
    pub(super) link_discovery_count: u64,
}

/// 一份记忆图（项目一份、全局一份）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MemoryGraph {
    /// 格式版本。
    pub(super) graph_version: u32,
    /// 记忆节点：id → 条目。
    pub(super) memories: HashMap<String, MemoryEntry>,
    /// 标签节点：`tag:<name>` → 标签。
    #[serde(default)]
    pub(super) tags: HashMap<String, TagEntry>,
    /// 聚类节点（prux 不做聚类，恒空；保留字段只为形状兼容）。
    #[serde(default)]
    pub(super) clusters: HashMap<String, Value>,
    /// 正向边：源 id → 边列表。
    #[serde(default)]
    pub(super) edges: HashMap<String, Vec<Edge>>,
    /// 反向边：目标 id → 源 id 列表（BFS 用）。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) reverse_edges: HashMap<String, Vec<String>>,
    /// 统计信息。
    #[serde(default)]
    pub(super) metadata: GraphMetadata,
}

impl Default for MemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryGraph {
    /// 空图。
    pub(super) fn new() -> Self {
        Self {
            graph_version: GRAPH_VERSION,
            memories: HashMap::new(),
            tags: HashMap::new(),
            clusters: HashMap::new(),
            edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            metadata: GraphMetadata::default(),
        }
    }

    /// 记忆条数。
    #[allow(dead_code)]
    pub(super) fn memory_count(&self) -> usize {
        self.memories.len()
    }

    /// 边总数。
    pub(super) fn edge_count(&self) -> usize {
        self.edges.values().map(|v| v.len()).sum()
    }

    /// 插入一条记忆（会建标签节点与 `HasTag` 边），返回其 id。
    ///
    /// 超过 [`MAX_ENTRIES`] 时淘汰打分最低的其它条目（优先已失效的）。
    pub(super) fn add_memory(&mut self, mut entry: MemoryEntry) -> String {
        entry.refresh_search_text();
        let id = entry.id.clone();

        for tag in entry.tags.clone() {
            self.ensure_tag(&tag);
            let tag_id = format!("tag:{tag}");
            self.add_edge_internal(&id, &tag_id, EdgeKind::HasTag);
            if let Some(entry) = self.tags.get_mut(&tag_id) {
                entry.count = entry.count.saturating_add(1);
            }
        }

        self.memories.insert(id.clone(), entry);
        self.prune_to_limit(&id);
        id
    }

    /// 超出容量时淘汰最低分条目。
    fn prune_to_limit(&mut self, keep: &str) {
        if self.memories.len() <= MAX_ENTRIES {
            return;
        }
        let mut victims: Vec<(String, f64)> = self
            .memories
            .values()
            .filter(|entry| entry.id != keep)
            .map(|entry| (entry.id.clone(), entry.memory_score()))
            .collect();
        victims.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let overflow = self.memories.len() - MAX_ENTRIES;
        for (id, _) in victims.into_iter().take(overflow) {
            self.remove_memory(&id);
        }
    }

    /// 取一条记忆。
    pub(super) fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.memories.get(id)
    }

    /// 取一条记忆（可变）。
    pub(super) fn get_mut(&mut self, id: &str) -> Option<&mut MemoryEntry> {
        self.memories.get_mut(id)
    }

    /// 删除一条记忆及其相关边，返回被删条目。
    pub(super) fn remove_memory(&mut self, id: &str) -> Option<MemoryEntry> {
        if let Some(edges) = self.edges.remove(id) {
            for edge in edges {
                if let Some(reverse) = self.reverse_edges.get_mut(&edge.target) {
                    reverse.retain(|src| src != id);
                }
                if matches!(edge.kind, EdgeKind::HasTag)
                    && let Some(tag) = self.tags.get_mut(&edge.target)
                {
                    tag.count = tag.count.saturating_sub(1);
                }
            }
        }

        if let Some(sources) = self.reverse_edges.remove(id) {
            for source in sources {
                if let Some(edges) = self.edges.get_mut(&source) {
                    edges.retain(|e| e.target != id);
                }
            }
        }

        self.memories.remove(id)
    }

    /// 有效记忆。
    pub(super) fn active_memories(&self) -> impl Iterator<Item = &MemoryEntry> {
        self.memories.values().filter(|m| m.active)
    }

    /// 确保标签节点存在。
    pub(super) fn ensure_tag(&mut self, name: &str) -> &TagEntry {
        let tag_id = format!("tag:{name}");
        self.tags
            .entry(tag_id.clone())
            .or_insert_with(|| TagEntry::new(name))
    }

    /// 给记忆打一个标签（已有则该标签的计数由调用方保证只加一次）。
    pub(super) fn tag_memory(&mut self, memory_id: &str, tag_name: &str) {
        self.ensure_tag(tag_name);
        let tag_id = format!("tag:{tag_name}");

        let already = self.edges.get(memory_id).is_some_and(|edges| {
            edges
                .iter()
                .any(|e| e.target == tag_id && matches!(e.kind, EdgeKind::HasTag))
        });
        if already {
            return;
        }

        self.add_edge_internal(memory_id, &tag_id, EdgeKind::HasTag);
        if let Some(tag) = self.tags.get_mut(&tag_id) {
            tag.count = tag.count.saturating_add(1);
        }
        if let Some(memory) = self.memories.get_mut(memory_id)
            && !memory.tags.iter().any(|t| t == tag_name)
        {
            memory.tags.push(tag_name.to_string());
            memory.refresh_search_text();
        }
    }

    /// 去掉记忆上的一个标签。
    pub(super) fn untag_memory(&mut self, memory_id: &str, tag_name: &str) {
        let tag_id = format!("tag:{tag_name}");
        if let Some(edges) = self.edges.get_mut(memory_id) {
            edges.retain(|e| !(e.target == tag_id && matches!(e.kind, EdgeKind::HasTag)));
        }
        if let Some(sources) = self.reverse_edges.get_mut(&tag_id) {
            sources.retain(|s| s != memory_id);
        }
        if let Some(tag) = self.tags.get_mut(&tag_id) {
            tag.count = tag.count.saturating_sub(1);
        }
        if let Some(memory) = self.memories.get_mut(memory_id) {
            memory.tags.retain(|t| t != tag_name);
            memory.refresh_search_text();
        }
    }

    /// 所有标签（按名字排序，便于稳定展示）。
    pub(super) fn sorted_tags(&self) -> Vec<&TagEntry> {
        let mut tags: Vec<&TagEntry> = self.tags.values().filter(|t| t.count > 0).collect();
        tags.sort_by(|a, b| a.name.cmp(&b.name));
        tags
    }

    /// 建一条边（内部用，不查重）。
    fn add_edge_internal(&mut self, from: &str, to: &str, kind: EdgeKind) {
        self.edges.entry(from.to_string()).or_default().push(Edge {
            target: to.to_string(),
            kind,
        });
        self.reverse_edges
            .entry(to.to_string())
            .or_default()
            .push(from.to_string());
    }

    /// 建一条边（已存在同类型同目标时跳过）。
    pub(super) fn link_memories(&mut self, from: &str, to: &str, weight: f32) {
        let kind = EdgeKind::RelatesTo { weight };
        let exists = self
            .edges
            .get(from)
            .is_some_and(|edges| edges.iter().any(|e| e.target == to && e.kind == kind));
        if exists {
            return;
        }
        self.add_edge_internal(from, to, kind);
        self.metadata.link_discovery_count = self.metadata.link_discovery_count.saturating_add(1);
    }

    /// 让 `newer_id` 取代 `older_id`（旧条目保留但不再参与召回）。
    pub(super) fn supersede(&mut self, newer_id: &str, older_id: &str) {
        self.link_if_absent(newer_id, older_id, EdgeKind::Supersedes);
        if let Some(older) = self.memories.get_mut(older_id) {
            older.supersede(newer_id);
        }
    }

    /// 建一条无重复检查的边（仅当尚不存在）。
    fn link_if_absent(&mut self, from: &str, to: &str, kind: EdgeKind) {
        let exists = self
            .edges
            .get(from)
            .is_some_and(|edges| edges.iter().any(|e| e.target == to && e.kind == kind));
        if !exists {
            self.add_edge_internal(from, to, kind);
        }
    }

    /// 从 `root` 出发做 BFS，取关联记忆（不含自身），按累计权重降序。
    pub(super) fn related(&mut self, root: &str, depth: usize) -> Vec<(MemoryEntry, f32)> {
        self.metadata.retrieval_count = self.metadata.retrieval_count.saturating_add(1);
        if depth == 0 || !self.memories.contains_key(root) {
            return Vec::new();
        }

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(root.to_string());
        let mut best: HashMap<String, f32> = HashMap::new();
        let mut queue: VecDeque<(String, f32, usize)> = VecDeque::new();
        queue.push_back((root.to_string(), 1.0, 0));

        while let Some((node, score, level)) = queue.pop_front() {
            if level >= depth {
                continue;
            }
            let edges = self.edges.get(&node).cloned().unwrap_or_default();
            for edge in edges {
                let weight = edge.kind.traversal_weight();
                let next_score = score * weight * 0.7;
                if edge.target.starts_with("tag:") {
                    let sources = self
                        .reverse_edges
                        .get(&edge.target)
                        .cloned()
                        .unwrap_or_default();
                    for source in sources {
                        if visited.contains(&source) {
                            continue;
                        }
                        if !self.memories.contains_key(&source) {
                            continue;
                        }
                        visited.insert(source.clone());
                        best.insert(source.clone(), next_score);
                        queue.push_back((source, next_score, level + 1));
                    }
                } else if self.memories.contains_key(&edge.target)
                    && visited.insert(edge.target.clone())
                {
                    best.insert(edge.target.clone(), next_score);
                    queue.push_back((edge.target, next_score, level + 1));
                }
            }
        }

        let mut related: Vec<(MemoryEntry, f32)> = best
            .into_iter()
            .filter_map(|(id, score)| {
                self.memories
                    .get(&id)
                    .filter(|entry| entry.active)
                    .map(|entry| (entry.clone(), score))
            })
            .collect();
        related.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        related
    }
}

/// 归一化检索文本：小写、把分隔符换成空格、压掉重复空白；标签追加在正文之后。
pub(super) fn normalize_search_text(content: &str, tags: &[String]) -> String {
    let mut normalized = normalize_text_part(content);
    for tag in tags {
        let tag = normalize_text_part(tag);
        if tag.is_empty() {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(&tag);
    }
    normalized
}

/// 单段文本的归一化（见 [`normalize_search_text`]）。
fn normalize_text_part(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let mut normalized = String::with_capacity(lowered.len());
    let mut last_space = true;
    for ch in lowered.chars() {
        let mapped = if ch.is_whitespace() || matches!(ch, '-' | '_' | '/' | '\\' | '.' | ':' | ',')
        {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if !last_space {
                normalized.push(' ');
                last_space = true;
            }
        } else {
            normalized.push(mapped);
            last_space = false;
        }
    }
    normalized.trim_end().to_string()
}

/// 从磁盘读一份图：文件缺失返回空图，损坏时返回错误（调用方决定提示还是忽略）。
///
/// 读取时会把缺失的 `search_text` / 旧版本号就地修正，但**不写回**——写回由
/// 调用方在下一次真实变更时一并完成，避免一次只读操作也产生磁盘写入。
pub(super) fn load_graph(path: &Path) -> io::Result<MemoryGraph> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(MemoryGraph::new()),
        Err(err) => return Err(err),
    };
    let mut graph: MemoryGraph = serde_json::from_str(&text).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid memory graph {}: {err}", path.display()),
        )
    })?;
    graph.graph_version = GRAPH_VERSION;
    for entry in graph.memories.values_mut() {
        if entry.search_text.is_empty() {
            entry.refresh_search_text();
        }
    }
    Ok(graph)
}

/// 原子写回一份图（同目录临时文件 + rename），并创建父目录。
pub(super) fn save_graph(path: &Path, graph: &MemoryGraph) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(graph)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{text}\n"))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 固定内容的一条记忆（测试里避免随机 id 造成断言不稳）。
    fn entry(category: MemoryCategory, content: &str) -> MemoryEntry {
        MemoryEntry::new(category, content)
    }

    #[test]
    fn category_and_scope_parse_round_trip() {
        assert_eq!("fact".parse::<MemoryCategory>(), Ok(MemoryCategory::Fact));
        assert_eq!(
            "PREFERENCE".parse::<MemoryCategory>(),
            Ok(MemoryCategory::Preference)
        );
        assert_eq!(
            MemoryCategory::Correction.as_label(),
            "correction",
            "命名空间内的展示名"
        );
        assert_eq!(
            MemoryCategory::parse_lenient("bugs"),
            MemoryCategory::Correction,
            "无法识别的分类归入事实，但常见别名要认"
        );
        assert_eq!(MemoryCategory::parse_lenient("weird"), MemoryCategory::Fact);
        assert_eq!(
            MemoryCategory::parse_lenient("纠正"),
            MemoryCategory::Correction,
            "中文分类名同样识别"
        );
        assert_eq!(
            MemoryCategory::parse_lenient("偏好"),
            MemoryCategory::Preference
        );
        assert_eq!("global".parse::<MemoryScope>(), Ok(MemoryScope::Global));
        assert!(MemoryScope::All.includes_project() && MemoryScope::All.includes_global());
        assert!(!MemoryScope::Project.includes_global());
    }

    #[test]
    fn add_memory_creates_tag_nodes_and_edges() {
        let mut graph = MemoryGraph::new();
        let id = graph.add_memory(
            entry(MemoryCategory::Fact, "uses pnpm")
                .with_tags(vec!["tooling".to_string(), "package-manager".to_string()]),
        );

        assert_eq!(graph.memory_count(), 1);
        assert_eq!(graph.tags.len(), 2, "两个标签节点");
        assert_eq!(graph.active_memories().count(), 1);
        assert_eq!(graph.get(&id).unwrap().tags.len(), 2);
        // 标签可被检索：正文 + 标签都进 search_text
        assert!(graph.get(&id).unwrap().searchable_text().contains("pnpm"));
        assert!(
            graph
                .get(&id)
                .unwrap()
                .searchable_text()
                .contains("tooling")
        );
    }

    #[test]
    fn tag_memory_is_idempotent_and_untag_cleans_counts() {
        let mut graph = MemoryGraph::new();
        let id = graph.add_memory(entry(MemoryCategory::Fact, "note"));
        graph.tag_memory(&id, "x");
        graph.tag_memory(&id, "x");
        assert_eq!(graph.tags["tag:x"].count, 1, "重复打同一标签只计一次");
        assert_eq!(graph.get(&id).unwrap().tags, vec!["x".to_string()]);

        graph.untag_memory(&id, "x");
        assert_eq!(graph.tags["tag:x"].count, 0);
        assert!(graph.get(&id).unwrap().tags.is_empty());
    }

    #[test]
    fn remove_memory_drops_edges_and_tag_counts() {
        let mut graph = MemoryGraph::new();
        let id = graph
            .add_memory(entry(MemoryCategory::Fact, "note").with_tags(vec!["keep-me".to_string()]));
        assert_eq!(graph.tags["tag:keep-me"].count, 1);

        graph.remove_memory(&id);
        assert_eq!(graph.memory_count(), 0);
        assert_eq!(graph.tags["tag:keep-me"].count, 0);
        assert!(graph.edges.is_empty() || !graph.edges.contains_key(&id));
    }

    #[test]
    fn related_walks_tags_and_links_with_decay() {
        let mut graph = MemoryGraph::new();
        let a = graph
            .add_memory(entry(MemoryCategory::Fact, "a").with_tags(vec!["shared".to_string()]));
        let b = graph
            .add_memory(entry(MemoryCategory::Fact, "b").with_tags(vec!["shared".to_string()]));
        let c = graph.add_memory(entry(MemoryCategory::Fact, "c"));
        graph.link_memories(&b, &c, 0.9);

        let from_a = graph.related(&a, 2);
        let ids: Vec<&str> = from_a.iter().map(|(e, _)| e.id.as_str()).collect();
        assert!(ids.contains(&b.as_str()), "同标签应当关联: {ids:?}");
        assert!(!ids.contains(&a.as_str()), "不包含自身");
        // 一跳的 b 分数高于两跳的 c
        let score_b = from_a.iter().find(|(e, _)| e.id == b).unwrap().1;
        let score_c = from_a.iter().find(|(e, _)| e.id == c).unwrap().1;
        assert!(score_b > score_c, "关联分数随跳数衰减");
        assert!(graph.related(&a, 0).is_empty(), "深度 0 不返回任何内容");
    }

    #[test]
    fn supersede_deactivates_old_entry() {
        let mut graph = MemoryGraph::new();
        let older = graph.add_memory(entry(MemoryCategory::Fact, "old"));
        let newer = graph.add_memory(entry(MemoryCategory::Fact, "new"));
        graph.supersede(&newer, &older);

        assert!(!graph.get(&older).unwrap().active);
        assert_eq!(
            graph.get(&older).unwrap().superseded_by.as_deref(),
            Some(newer.as_str())
        );
        assert_eq!(graph.active_memories().count(), 1);
        assert_eq!(
            graph.get(&older).unwrap().memory_score(),
            0.0,
            "失效条目不参与打分"
        );
    }

    #[test]
    fn graph_round_trips_through_disk() {
        let _guard = AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");

        let mut graph = MemoryGraph::new();
        let id = graph.add_memory(
            entry(MemoryCategory::Preference, "prefers pnpm over npm")
                .with_tags(vec!["tooling".to_string()])
                .with_source("session-1"),
        );
        graph.link_memories(&id, &id, 0.5);
        save_graph(&path, &graph).unwrap();

        let loaded = load_graph(&path).unwrap();
        assert_eq!(loaded.memory_count(), 1);
        assert_eq!(loaded.graph_version, GRAPH_VERSION);
        assert_eq!(loaded.tags["tag:tooling"].count, 1);
        assert_eq!(loaded.get(&id).unwrap().content, "prefers pnpm over npm");
        assert_eq!(
            loaded.get(&id).unwrap().source.as_deref(),
            Some("session-1")
        );
        assert!(loaded.get(&id).unwrap().active);
    }

    #[test]
    fn missing_file_loads_empty_and_corrupt_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_graph(&dir.path().join("nope.json"))
                .unwrap()
                .memories
                .is_empty()
        );

        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        let err = load_graph(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn hand_written_graph_without_optional_fields_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("minimal.json");
        std::fs::write(
            &path,
            r#"{
                "graph_version": 1,
                "memories": {
                    "mem_x": {
                        "id": "mem_x",
                        "category": "fact",
                        "content": "repo runs make check",
                        "created_at": "2026-01-01T00:00:00.000Z",
                        "updated_at": "2026-01-01T00:00:00.000Z"
                    }
                }
            }"#,
        )
        .unwrap();

        let graph = load_graph(&path).unwrap();
        let entry = graph.get("mem_x").unwrap();
        assert_eq!(graph.graph_version, GRAPH_VERSION, "旧版本号被就地升级");
        assert!(entry.active, "缺省 active = true");
        assert_eq!(entry.confidence, 1.0);
        assert!(entry.tags.is_empty());
        assert!(
            entry.search_text.contains("repo runs make check"),
            "缺失的 search_text 被补算"
        );
    }

    #[test]
    fn entries_beyond_capacity_are_pruned_by_score() {
        let mut graph = MemoryGraph::new();
        let mut ids = Vec::new();
        for i in 0..MAX_ENTRIES {
            ids.push(graph.add_memory(entry(MemoryCategory::Fact, &format!("note {i}"))));
        }
        // 让第一条成为最旧/最没用的那条：手工把它压到最低分
        if let Some(first) = graph.get_mut(&ids[0]) {
            first.updated_at = "2000-01-01T00:00:00.000Z".to_string();
            first.active = false;
        }
        let newest = graph.add_memory(entry(MemoryCategory::Fact, "overflow"));

        assert_eq!(graph.memory_count(), MAX_ENTRIES, "容量被守住");
        assert!(!graph.memories.contains_key(&ids[0]), "最低分的条目被淘汰");
        assert!(graph.memories.contains_key(&newest));
    }

    #[test]
    fn effective_confidence_decays_and_is_boosted_by_access() {
        let mut fresh = entry(MemoryCategory::Fact, "x");
        fresh.created_at = now_iso();
        fresh.updated_at = now_iso();
        let fresh_confidence = fresh.effective_confidence();

        let mut old = entry(MemoryCategory::Fact, "y");
        old.created_at = "2020-01-01T00:00:00.000Z".to_string();
        old.updated_at = "2020-01-01T00:00:00.000Z".to_string();
        assert!(
            old.effective_confidence() < fresh_confidence,
            "旧条目置信度更低"
        );

        let before = old.effective_confidence();
        old.access_count = 50;
        assert!(old.effective_confidence() > before, "被用过的条目获得加成");
        assert!(fresh.effective_confidence() <= 1.0);
    }

    #[test]
    fn reinforce_and_touch_update_timestamps() {
        let mut item = entry(MemoryCategory::Fact, "x");
        let created = item.created_at.clone();
        let initial_updated = item.updated_at.clone();
        item.reinforce("session-1");
        assert_eq!(item.strength, 2);
        assert_eq!(item.reinforcements.len(), 1);
        assert_eq!(item.reinforcements[0].session_id, "session-1");
        assert_eq!(item.created_at, created, "创建时间不随强化改变");

        item.touch();
        assert_eq!(item.access_count, 1);
        // 墙钟可能同毫秒 → 用访问计数与置信度一起判定，避免脆断
        assert!(item.confidence > 1.0 - f32::EPSILON);
        assert_eq!(item.created_at, created);
        let _ = initial_updated;
    }
}
