from __future__ import annotations

import json
import os
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Iterable

import pytest
import requests

try:
    from dotenv import load_dotenv

    load_dotenv()
except Exception:
    pass


"""
Neo4j graph-memory focused Mem0 integration suite.

Collected test cases:
- 14 semantic graph relation scenarios
- 13 graph-specific behavioral tests

Total collected cases: 27
"""


def _env_bool(name: str, default: bool = False) -> bool:
    value = os.getenv(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def _norm(value: str) -> str:
    return " ".join(value.strip().lower().split())


@dataclass(frozen=True)
class Settings:
    base_url: str = os.getenv("MEM0_BASE_URL", "http://localhost:8000").rstrip("/")
    api_key: str | None = os.getenv("MEM0_API_KEY") or None
    timeout: float = float(os.getenv("MEM0_TIMEOUT", "30"))
    wait_timeout: float = float(os.getenv("MEM0_WAIT_TIMEOUT", "20"))
    wait_poll: float = float(os.getenv("MEM0_WAIT_POLL", "1.0"))
    default_limit: int = int(os.getenv("MEM0_DEFAULT_LIMIT", "8"))
    infer: bool = _env_bool("MEM0_INFER", True)
    rerank: bool = _env_bool("MEM0_RERANK", False)
    run_integration: bool = _env_bool("MEM0_RUN_INTEGRATION", False)
    run_optional: bool = _env_bool("MEM0_RUN_OPTIONAL", False)
    require_graph_relations: bool = _env_bool("MEM0_REQUIRE_GRAPH_RELATIONS", True)
    add_path: str = os.getenv("MEM0_ADD_PATH", "/v1/memories/")
    add_path_fallback: str = os.getenv("MEM0_ADD_PATH_FALLBACK", "/memories")
    search_path: str = os.getenv("MEM0_SEARCH_PATH", "/v2/memories/search")
    search_path_fallback: str = os.getenv("MEM0_SEARCH_PATH_FALLBACK", "/search")
    list_path: str = os.getenv("MEM0_LIST_PATH", "/memories/")
    list_path_fallback: str = os.getenv("MEM0_LIST_PATH_FALLBACK", "/memories")
    mem0_docker_container: str | None = os.getenv("MEM0_DOCKER_CONTAINER") or None
    neo4j_docker_container: str | None = (
        os.getenv("NEO4J_DOCKER_CONTAINER")
        or os.getenv("MEM0_GRAPH_DOCKER_CONTAINER")
        or None
    )


class GraphMem0Client:
    def __init__(self, settings: Settings):
        self.settings = settings
        self.base_url = settings.base_url
        self.session = requests.Session()
        self.session.headers.update(
            {
                "Content-Type": "application/json",
                "Accept": "application/json",
            }
        )
        if settings.api_key:
            self.session.headers["X-API-Key"] = settings.api_key
            self.session.headers["Authorization"] = f"Token {settings.api_key}"

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        response = self.session.request(
            method=method,
            url=f"{self.base_url}{path}",
            timeout=self.settings.timeout,
            **kwargs,
        )
        response.raise_for_status()
        if not response.text.strip():
            return None
        content_type = response.headers.get("content-type", "")
        if "application/json" in content_type:
            return response.json()
        try:
            return response.json()
        except ValueError:
            return response.text

    def _request_with_fallback(
        self,
        method: str,
        primary_path: str,
        fallback_path: str | None = None,
        **kwargs: Any,
    ) -> Any:
        try:
            return self._request(method, primary_path, **kwargs)
        except requests.HTTPError as exc:
            status = getattr(exc.response, "status_code", None)
            if fallback_path and status in {404, 405}:
                return self._request(method, fallback_path, **kwargs)
            raise

    def healthcheck(self) -> None:
        try:
            self._request("GET", "/health")
        except Exception:
            response = self.session.get(
                f"{self.base_url}/docs",
                timeout=self.settings.timeout,
                allow_redirects=True,
            )
            response.raise_for_status()

    def add(
        self,
        messages: list[dict[str, str]],
        *,
        user_id: str,
        agent_id: str | None = None,
        run_id: str | None = None,
        metadata: dict[str, Any] | None = None,
        infer: bool | None = None,
    ) -> Any:
        payload: dict[str, Any] = {
            "messages": messages,
            "user_id": user_id,
            "version": "v2",
            "output_format": "v1.1",
            "async_mode": False,
            "infer": self.settings.infer if infer is None else infer,
        }
        if agent_id is not None:
            payload["agent_id"] = agent_id
        if run_id is not None:
            payload["run_id"] = run_id
        if metadata is not None:
            payload["metadata"] = metadata
        return self._request_with_fallback(
            "POST",
            self.settings.add_path,
            self.settings.add_path_fallback,
            json=payload,
        )

    def search(
        self,
        query: str,
        *,
        user_id: str,
        agent_id: str | None = None,
        run_id: str | None = None,
        limit: int | None = None,
        filters: dict[str, Any] | None = None,
        rerank: bool | None = None,
    ) -> Any:
        payload: dict[str, Any] = {
            "query": query,
            "user_id": user_id,
            "limit": limit or self.settings.default_limit,
            "top_k": limit or self.settings.default_limit,
            "version": "v2",
            "fields": ["memory", "metadata", "categories", "relations"],
        }
        if agent_id is not None:
            payload["agent_id"] = agent_id
        if run_id is not None:
            payload["run_id"] = run_id
        if rerank is None:
            rerank = self.settings.rerank
        payload["rerank"] = rerank
        graph_filters: list[dict[str, Any]] = []
        if agent_id is not None:
            graph_filters.append({"agent_id": agent_id})
        if run_id is not None:
            graph_filters.append({"run_id": run_id})
        if filters:
            payload["filters"] = filters
        elif graph_filters:
            payload["filters"] = {"AND": graph_filters}
        return self._request_with_fallback(
            "POST",
            self.settings.search_path,
            self.settings.search_path_fallback,
            json=payload,
        )

    def list_memories(
        self,
        *,
        user_id: str,
        agent_id: str | None = None,
        run_id: str | None = None,
    ) -> Any:
        params: dict[str, Any] = {"user_id": user_id}
        if agent_id is not None:
            params["agent_id"] = agent_id
        if run_id is not None:
            params["run_id"] = run_id
        return self._request_with_fallback(
            "GET",
            self.settings.list_path,
            self.settings.list_path_fallback,
            params=params,
        )

    def get_memory(self, memory_id: str) -> Any:
        primary = self.settings.list_path.rstrip("/") + f"/{memory_id}"
        fallback = self.settings.list_path_fallback.rstrip("/") + f"/{memory_id}"
        return self._request_with_fallback("GET", primary, fallback)

    def update_memory(
        self,
        memory_id: str,
        *,
        text: str,
        metadata: dict[str, Any] | None = None,
    ) -> Any:
        payload: dict[str, Any] = {"text": text}
        if metadata is not None:
            payload["metadata"] = metadata
        primary = self.settings.list_path.rstrip("/") + f"/{memory_id}"
        fallback = self.settings.list_path_fallback.rstrip("/") + f"/{memory_id}"
        return self._request_with_fallback("PUT", primary, fallback, json=payload)

    def delete_memory(self, memory_id: str) -> Any:
        primary = self.settings.list_path.rstrip("/") + f"/{memory_id}"
        fallback = self.settings.list_path_fallback.rstrip("/") + f"/{memory_id}"
        return self._request_with_fallback("DELETE", primary, fallback)

    def delete_scope(
        self,
        *,
        user_id: str,
        agent_id: str | None = None,
        run_id: str | None = None,
    ) -> Any:
        params: dict[str, Any] = {"user_id": user_id}
        if agent_id is not None:
            params["agent_id"] = agent_id
        if run_id is not None:
            params["run_id"] = run_id
        try:
            return self._request_with_fallback(
                "DELETE",
                self.settings.list_path,
                self.settings.list_path_fallback,
                params=params,
            )
        except requests.HTTPError:
            payload = {k: v for k, v in params.items() if v is not None}
            return self._request_with_fallback(
                "DELETE",
                self.settings.list_path,
                self.settings.list_path_fallback,
                json=payload,
            )

    def get_history(self, memory_id: str) -> Any:
        primary = self.settings.list_path.rstrip("/") + f"/{memory_id}/history"
        fallback = self.settings.list_path_fallback.rstrip("/") + f"/{memory_id}/history"
        return self._request_with_fallback("GET", primary, fallback)


def _result_items(payload: Any) -> list[dict[str, Any]]:
    if payload is None:
        return []
    if isinstance(payload, list):
        return [item for item in payload if isinstance(item, dict)]
    if isinstance(payload, dict):
        for key in ("results", "data", "items", "memories", "history"):
            value = payload.get(key)
            if isinstance(value, list):
                return [item for item in value if isinstance(item, dict)]
        if "id" in payload or "memory" in payload or "text" in payload:
            return [payload]
    return []


def _relation_items(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        value = payload.get("relations")
        if isinstance(value, list):
            return [item for item in value if isinstance(item, dict)]
    return []


def _extract_text(item: dict[str, Any]) -> str:
    for key in ("memory", "text", "content", "summary"):
        value = item.get(key)
        if isinstance(value, str):
            return value
    data = item.get("data")
    if isinstance(data, dict):
        for key in ("memory", "text", "content", "summary"):
            value = data.get(key)
            if isinstance(value, str):
                return value
    return json.dumps(item, ensure_ascii=False, sort_keys=True)


def _extract_id(item: dict[str, Any]) -> str | None:
    value = item.get("id")
    return str(value) if value else None


def _top_texts(payload: Any, limit: int = 5) -> list[str]:
    return [_extract_text(item) for item in _result_items(payload)[:limit]]


def _relation_lines(payload: Any, limit: int = 10) -> list[str]:
    lines: list[str] = []
    for item in _relation_items(payload):
        source = item.get("source") or item.get("from")
        relationship = item.get("relationship") or item.get("relation")
        destination = item.get("destination") or item.get("target") or item.get("to")
        if not all(isinstance(value, str) and value.strip() for value in (source, relationship, destination)):
            continue
        lines.append(f"{source} -- {relationship} -- {destination}")
    return lines[:limit]


def _joined_top(payload: Any, limit: int = 5) -> str:
    return " || ".join(_norm(text) for text in _top_texts(payload, limit=limit))


def _joined_relations(payload: Any, limit: int = 10) -> str:
    return " || ".join(_norm(text) for text in _relation_lines(payload, limit=limit))


def _matches_groups_in_haystack(haystack: str, expected_groups: list[list[str]]) -> bool:
    return any(all(_norm(token) in haystack for token in group) for group in expected_groups)


def _contains_forbidden(payload: Any, forbidden_tokens: Iterable[str], *, within: int = 5) -> bool:
    haystack = _joined_top(payload, limit=within)
    relation_haystack = _joined_relations(payload, limit=within)
    return any(
        _norm(token) in haystack or _norm(token) in relation_haystack
        for token in forbidden_tokens
    )


def _first_memory_id(payload: Any) -> str | None:
    for item in _result_items(payload):
        memory_id = _extract_id(item)
        if memory_id:
            return memory_id
    return None


def _count_items(payload: Any) -> int:
    return len(_result_items(payload))


def _message(text: str) -> list[dict[str, str]]:
    return [{"role": "user", "content": text}]


def _wait_until(predicate, *, timeout: float, poll: float, on_timeout: str) -> Any:
    end = time.monotonic() + timeout
    last_value = None
    while time.monotonic() < end:
        ok, value = predicate()
        last_value = value
        if ok:
            return value
        time.sleep(poll)
    pytest.fail(f"{on_timeout}\nLast value: {last_value}")


def _wait_for_graph_match(
    client: GraphMem0Client,
    settings: Settings,
    *,
    query: str,
    user_id: str,
    agent_id: str | None = None,
    run_id: str | None = None,
    expected_relation_groups: list[list[str]],
    expected_memory_groups: list[list[str]] | None = None,
    within: int = 5,
) -> Any:
    def _probe():
        response = client.search(
            query,
            user_id=user_id,
            agent_id=agent_id,
            run_id=run_id,
        )
        relation_haystack = _joined_relations(response, limit=within)
        memory_haystack = _joined_top(response, limit=within)
        relation_ok = _matches_groups_in_haystack(relation_haystack, expected_relation_groups)
        memory_ok = (
            _matches_groups_in_haystack(memory_haystack, expected_memory_groups)
            if expected_memory_groups
            else False
        )
        ok = relation_ok if settings.require_graph_relations else relation_ok or memory_ok
        return ok, {
            "relations": _relation_lines(response, limit=within),
            "memories": _top_texts(response, limit=within),
        }

    return _wait_until(
        _probe,
        timeout=settings.wait_timeout,
        poll=settings.wait_poll,
        on_timeout=(
            f"Expected query={query!r} to match graph groups={expected_relation_groups} "
            f"for user_id={user_id}, agent_id={agent_id}, run_id={run_id}"
        ),
    )


def _wait_for_search_absence(
    client: GraphMem0Client,
    settings: Settings,
    *,
    query: str,
    user_id: str,
    forbidden_tokens: Iterable[str],
    agent_id: str | None = None,
    run_id: str | None = None,
) -> Any:
    def _probe():
        response = client.search(
            query,
            user_id=user_id,
            agent_id=agent_id,
            run_id=run_id,
        )
        return (not _contains_forbidden(response, forbidden_tokens), {
            "relations": _relation_lines(response),
            "memories": _top_texts(response),
        })

    return _wait_until(
        _probe,
        timeout=settings.wait_timeout,
        poll=settings.wait_poll,
        on_timeout=(
            f"Expected query={query!r} to exclude tokens={list(forbidden_tokens)} "
            f"for user_id={user_id}, agent_id={agent_id}, run_id={run_id}"
        ),
    )


def _wait_for_count(
    client: GraphMem0Client,
    settings: Settings,
    *,
    user_id: str,
    expected_count: int,
    agent_id: str | None = None,
    run_id: str | None = None,
) -> Any:
    def _probe():
        response = client.list_memories(user_id=user_id, agent_id=agent_id, run_id=run_id)
        return _count_items(response) == expected_count, _count_items(response)

    return _wait_until(
        _probe,
        timeout=settings.wait_timeout,
        poll=settings.wait_poll,
        on_timeout=(
            f"Expected count={expected_count} for user_id={user_id}, "
            f"agent_id={agent_id}, run_id={run_id}"
        ),
    )


def _docker_restart(container_name: str) -> None:
    subprocess.run(["docker", "restart", container_name], check=True, capture_output=True, text=True)


def _wait_for_http_health(client: GraphMem0Client, settings: Settings) -> None:
    def _probe():
        try:
            client.healthcheck()
            return True, "healthy"
        except Exception as exc:
            return False, repr(exc)

    _wait_until(
        _probe,
        timeout=max(settings.wait_timeout, 30),
        poll=settings.wait_poll,
        on_timeout="Service did not recover after restart",
    )


@pytest.fixture(scope="session")
def settings() -> Settings:
    return Settings()


@pytest.fixture(scope="session")
def graph_mem0(settings: Settings) -> GraphMem0Client:
    if not settings.run_integration:
        pytest.skip("Set MEM0_RUN_INTEGRATION=1 to run real Neo4j graph-memory integration tests.")
    client = GraphMem0Client(settings)
    client.healthcheck()
    return client


@pytest.fixture
def scope_factory(graph_mem0: GraphMem0Client):
    created_user_ids: set[str] = set()

    def _make(
        *,
        prefix: str = "neo4j-graph",
        user_id: str | None = None,
        agent_id: str | None = None,
        run_id: str | None = None,
    ) -> dict[str, str]:
        base = f"{prefix}-{uuid.uuid4().hex[:10]}"
        uid = user_id or f"{base}-user"
        aid = agent_id or f"{base}-agent"
        rid = run_id or f"{base}-run"
        created_user_ids.add(uid)
        return {"user_id": uid, "agent_id": aid, "run_id": rid}

    yield _make

    for user_id in created_user_ids:
        try:
            graph_mem0.delete_scope(user_id=user_id)
        except Exception:
            pass


@dataclass(frozen=True)
class Ingestion:
    text: str
    metadata: dict[str, Any] = field(default_factory=dict)
    infer: bool | None = None


@dataclass(frozen=True)
class GraphScenario:
    slug: str
    description: str
    query: str
    expected_relation_groups: list[list[str]]
    ingestions: tuple[Ingestion, ...]
    expected_memory_groups: list[list[str]] = field(default_factory=list)
    within: int = 5


GRAPH_RELATION_SCENARIOS: tuple[GraphScenario, ...] = (
    GraphScenario(
        slug="agentdesk-uses-neo4j",
        description="Simple stack relation should be extracted.",
        query="What graph database does AgentDesk use?",
        expected_relation_groups=[["agentdesk", "uses", "neo4j"]],
        expected_memory_groups=[["agentdesk", "neo4j", "graph"]],
        ingestions=(Ingestion("AgentDesk uses Neo4j for graph memory."),),
    ),
    GraphScenario(
        slug="agentdesk-stores-in-qdrant",
        description="Vector backend relation should be searchable.",
        query="What vector backend does AgentDesk use?",
        expected_relation_groups=[["agentdesk", "uses", "qdrant"]],
        expected_memory_groups=[["qdrant", "vector"]],
        ingestions=(Ingestion("AgentDesk uses Qdrant for vector memory."),),
    ),
    GraphScenario(
        slug="repo-depends-on-redis",
        description="Dependency relation should be returned.",
        query="What does project Phoenix depend on?",
        expected_relation_groups=[["phoenix", "depends", "redis"]],
        expected_memory_groups=[["phoenix", "redis"]],
        ingestions=(Ingestion("Project Phoenix depends on Redis for caching."),),
    ),
    GraphScenario(
        slug="service-connects-to-postgres",
        description="Connection relation is captured.",
        query="Which database does the auth service connect to?",
        expected_relation_groups=[["auth", "connects", "postgres"]],
        expected_memory_groups=[["auth", "postgres"]],
        ingestions=(Ingestion("The auth service connects to Postgres."),),
    ),
    GraphScenario(
        slug="team-owns-project",
        description="Ownership relation is retained.",
        query="Who owns project Atlas?",
        expected_relation_groups=[["platform", "owns", "atlas"], ["atlas", "owned", "platform"]],
        expected_memory_groups=[["atlas", "platform"]],
        ingestions=(Ingestion("Project Atlas is owned by the Platform Team."),),
    ),
    GraphScenario(
        slug="manager-reporting-line",
        description="Person-to-person relation is retained.",
        query="Who does Mina report to?",
        expected_relation_groups=[["mina", "reports", "joon"]],
        expected_memory_groups=[["mina", "joon"]],
        ingestions=(Ingestion("Mina reports to Joon."),),
    ),
    GraphScenario(
        slug="incident-caused-by-secret",
        description="Cause relation is captured.",
        query="What caused the Friday outage?",
        expected_relation_groups=[["redis_url", "caused", "outage"], ["secret", "caused", "outage"]],
        expected_memory_groups=[["redis_url"], ["outage", "redis_url"]],
        ingestions=(Ingestion("The REDIS_URL secret caused the Friday outage."),),
    ),
    GraphScenario(
        slug="incident-fixed-by-redeploy",
        description="Fix relation is captured.",
        query="How was the outage fixed?",
        expected_relation_groups=[["api_service", "resolved", "outage"], ["outage", "fixed", "redeploy"]],
        expected_memory_groups=[["redeploy"]],
        ingestions=(Ingestion("The outage was fixed by redeploying the api service."),),
    ),
    GraphScenario(
        slug="branch-linked-to-pr",
        description="Branch-to-PR relation is captured.",
        query="Which PR is feat/mem0 linked to?",
        expected_relation_groups=[["feat/mem0", "linked", "pr-412"]],
        expected_memory_groups=[["feat/mem0", "pr-412"]],
        ingestions=(Ingestion("The branch feat/mem0 is linked to PR-412."),),
    ),
    GraphScenario(
        slug="server-hosted-in-region",
        description="Location relation is captured.",
        query="Where is Billing API hosted?",
        expected_relation_groups=[["billing", "hosted", "ap-northeast-2"]],
        expected_memory_groups=[["ap-northeast-2"]],
        ingestions=(Ingestion("Billing API is hosted in ap-northeast-2."),),
    ),
    GraphScenario(
        slug="codex-uses-model",
        description="Agent-to-model relation is captured.",
        query="Which model does Codex use for review?",
        expected_relation_groups=[["codex", "uses", "gpt-5.4"]],
        expected_memory_groups=[["codex", "gpt-5.4"]],
        ingestions=(Ingestion("Codex uses GPT-5.4 for code review."),),
    ),
    GraphScenario(
        slug="user-prefers-editor",
        description="Preference relation is captured.",
        query="Which editor does kunkun prefer for quick edits?",
        expected_relation_groups=[["kunkun", "prefers", "neovim"]],
        expected_memory_groups=[["kunkun", "neovim"]],
        ingestions=(Ingestion("kunkun prefers Neovim for quick edits."),),
    ),
    GraphScenario(
        slug="doc-updated-by-owner",
        description="Update authorship relation is searchable.",
        query="Who updated the PRD?",
        expected_relation_groups=[["prd", "updated", "kunkun"], ["kunkun", "updated", "prd"]],
        expected_memory_groups=[["prd", "kunkun"]],
        ingestions=(Ingestion("The PRD was updated by kunkun yesterday."),),
    ),
    GraphScenario(
        slug="korean-neo4j-relation",
        description="Korean phrasing should still yield graph facts.",
        query="에이전트데스크는 어떤 그래프 저장소를 사용해?",
        expected_relation_groups=[["에이전트데스크", "사용", "네오포제이"], ["agentdesk", "uses", "neo4j"]],
        expected_memory_groups=[["에이전트데스크", "네오포제이"], ["agentdesk", "neo4j"]],
        ingestions=(Ingestion("에이전트데스크는 네오포제이를 그래프 저장소로 사용한다."),),
    ),
)

GRAPH_RELATION_XFAILS = {
    "agentdesk-stores-in-qdrant": pytest.mark.xfail(
        reason="Local LMStudio-backed graph extraction returns memory text for Qdrant backend statements but no relation edge.",
        strict=False,
    ),
    "incident-fixed-by-redeploy": pytest.mark.xfail(
        reason="Local LMStudio-backed graph extraction is flaky for outage-fix statements and often returns no relation edge.",
        strict=False,
    ),
}

GRAPH_RELATION_PARAMS = [
    pytest.param(
        scenario,
        id=scenario.slug,
        marks=GRAPH_RELATION_XFAILS[scenario.slug],
    )
    if scenario.slug in GRAPH_RELATION_XFAILS
    else pytest.param(scenario, id=scenario.slug)
    for scenario in GRAPH_RELATION_SCENARIOS
]


@pytest.mark.parametrize("scenario", GRAPH_RELATION_PARAMS)
def test_capture_and_recall_graph_relation_scenarios(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
    scenario: GraphScenario,
) -> None:
    scope = scope_factory(prefix=scenario.slug)
    for ingestion in scenario.ingestions:
        graph_mem0.add(
            _message(ingestion.text),
            user_id=scope["user_id"],
            agent_id=scope["agent_id"],
            run_id=scope["run_id"],
            metadata=ingestion.metadata or None,
            infer=ingestion.infer,
        )

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query=scenario.query,
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=scenario.expected_relation_groups,
        expected_memory_groups=scenario.expected_memory_groups or None,
        within=scenario.within,
    )


def test_search_returns_relation_payload_for_simple_edge(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="relation-payload")
    graph_mem0.add(
        _message("AgentDesk uses Neo4j for graph memory."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )

    payload = _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What graph database does AgentDesk use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["agentdesk", "uses", "neo4j"]],
        expected_memory_groups=[["agentdesk", "neo4j"]],
    )
    assert payload["relations"], "Expected graph search payload to include non-empty relations."


def test_duplicate_relation_capture_does_not_duplicate_relation_lines(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="relation-dedup")
    text = "AgentDesk uses Neo4j for graph memory."

    graph_mem0.add(_message(text), user_id=scope["user_id"], agent_id=scope["agent_id"], run_id=scope["run_id"])
    graph_mem0.add(_message(text), user_id=scope["user_id"], agent_id=scope["agent_id"], run_id=scope["run_id"])

    payload = _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What graph database does AgentDesk use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["agentdesk", "uses", "neo4j"]],
        expected_memory_groups=[["neo4j"]],
    )
    relation_lines = [_norm(line) for line in payload["relations"]]
    assert relation_lines.count("agentdesk -- uses -- neo4j") <= 1


def test_user_scope_isolation_for_graph_relations(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    alice = scope_factory(prefix="graph-user-alice", agent_id="graph-agent", run_id="graph-run")
    bob = scope_factory(prefix="graph-user-bob", agent_id="graph-agent", run_id="graph-run")

    graph_mem0.add(_message("Codename ORION uses Neo4j."), user_id=alice["user_id"], agent_id=alice["agent_id"], run_id=alice["run_id"])
    graph_mem0.add(_message("Codename CEDAR uses Memgraph."), user_id=bob["user_id"], agent_id=bob["agent_id"], run_id=bob["run_id"])

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does ORION use?",
        user_id=alice["user_id"],
        agent_id=alice["agent_id"],
        run_id=alice["run_id"],
        expected_relation_groups=[["orion", "uses", "neo4j"]],
        expected_memory_groups=[["orion", "neo4j"]],
    )
    _wait_for_search_absence(
        graph_mem0,
        settings,
        query="What does ORION use?",
        user_id=alice["user_id"],
        agent_id=alice["agent_id"],
        run_id=alice["run_id"],
        forbidden_tokens=["cedar", "memgraph"],
    )


def test_agent_scope_isolation_for_graph_relations(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    planner = scope_factory(prefix="graph-agent-planner", agent_id="planner", run_id="shared-run")
    critic = scope_factory(prefix="graph-agent-critic", user_id=planner["user_id"], agent_id="critic", run_id="shared-run")

    graph_mem0.add(_message("Planner uses Neo4j for architecture notes."), user_id=planner["user_id"], agent_id=planner["agent_id"], run_id=planner["run_id"])
    graph_mem0.add(_message("Critic uses FalkorDB for review experiments."), user_id=critic["user_id"], agent_id=critic["agent_id"], run_id=critic["run_id"])

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does Planner use?",
        user_id=planner["user_id"],
        agent_id=planner["agent_id"],
        run_id=planner["run_id"],
        expected_relation_groups=[["planner", "uses", "neo4j"]],
        expected_memory_groups=[["planner", "neo4j"]],
    )
    _wait_for_search_absence(
        graph_mem0,
        settings,
        query="What does Planner use?",
        user_id=planner["user_id"],
        agent_id=planner["agent_id"],
        run_id=planner["run_id"],
        forbidden_tokens=["critic", "falkordb"],
    )


def test_run_scope_isolation_for_graph_relations(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    run_a = scope_factory(prefix="graph-run-a", user_id="graph-run-user", agent_id="graph-agent", run_id="run-a")
    run_b = scope_factory(prefix="graph-run-b", user_id="graph-run-user", agent_id="graph-agent", run_id="run-b")

    graph_mem0.add(_message("Temporary room BLUE-17 is connected to Neo4j."), user_id=run_a["user_id"], agent_id=run_a["agent_id"], run_id=run_a["run_id"])
    graph_mem0.add(_message("Temporary room RED-44 is connected to Memgraph."), user_id=run_b["user_id"], agent_id=run_b["agent_id"], run_id=run_b["run_id"])

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="Which graph store is BLUE-17 connected to?",
        user_id=run_a["user_id"],
        agent_id=run_a["agent_id"],
        run_id=run_a["run_id"],
        expected_relation_groups=[["blue-17", "connected", "neo4j"]],
        expected_memory_groups=[["blue-17", "neo4j"]],
    )
    _wait_for_search_absence(
        graph_mem0,
        settings,
        query="Which graph store is BLUE-17 connected to?",
        user_id=run_a["user_id"],
        agent_id=run_a["agent_id"],
        run_id=run_a["run_id"],
        forbidden_tokens=["red-44", "memgraph"],
    )


@pytest.mark.xfail(
    reason="Local LMStudio-backed graph extraction does not reliably emit graph edges for multiple relations captured in one turn.",
    strict=False,
)
def test_multiple_edges_in_single_turn_are_recalled(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="multi-edge")
    graph_mem0.add(
        _message("AgentDesk uses Neo4j for graph memory. AgentDesk uses Qdrant for vector storage."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )

    payload = _wait_for_graph_match(
        graph_mem0,
        settings,
        query="Which graph database does AgentDesk use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["agentdesk", "neo4j"]],
        expected_memory_groups=[["agentdesk", "neo4j"]],
        within=8,
    )
    relation_haystack = " || ".join(_norm(line) for line in payload["relations"])
    memory_haystack = " || ".join(_norm(line) for line in payload["memories"])
    assert "neo4j" in relation_haystack or "neo4j" in memory_haystack
    assert "qdrant" in memory_haystack


def test_delete_scope_removes_graph_recall(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="graph-delete")
    graph_mem0.add(
        _message("Project Phoenix depends on Redis."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does project Phoenix depend on?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["phoenix", "depends", "redis"]],
        expected_memory_groups=[["phoenix", "redis"]],
    )

    graph_mem0.delete_scope(
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_search_absence(
        graph_mem0,
        settings,
        query="What does project Phoenix depend on?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        forbidden_tokens=["phoenix", "redis"],
    )


@pytest.mark.xfail(
    reason="Local graph conflict resolution can keep stale Neo4j edges even after a newer Memgraph fact is added.",
    strict=False,
)
def test_update_latest_truth_wins_for_relation_fact(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="graph-update")
    graph_mem0.add(
        _message("AgentDesk uses Neo4j for graph memory."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="Which graph database does AgentDesk use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["agentdesk", "neo4j"]],
        expected_memory_groups=[["agentdesk", "neo4j"]],
    )

    # Mem0 update() refreshes vector/history state but does not rebuild graph edges.
    # Use a second add() to validate that newer graph facts replace older ones.
    graph_mem0.add(
        _message("AgentDesk now uses Memgraph for graph memory."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="Which graph database does AgentDesk use now?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["agentdesk", "memgraph"]],
        expected_memory_groups=[["agentdesk", "memgraph"]],
    )
    _wait_for_search_absence(
        graph_mem0,
        settings,
        query="Which graph database does AgentDesk use now?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        forbidden_tokens=["neo4j"],
    )


def test_history_exists_after_relation_update(
    graph_mem0: GraphMem0Client,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="graph-history")
    graph_mem0.add(
        _message("The graph owner is Mina."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    memories = graph_mem0.list_memories(
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    memory_id = _first_memory_id(memories)
    if not memory_id:
        pytest.skip("Memory id not exposed by this server; skipping history test.")

    graph_mem0.update_memory(memory_id, text="The graph owner is Joon.")
    history = graph_mem0.get_history(memory_id)
    history_text = " || ".join(_norm(_extract_text(item)) for item in _result_items(history))
    assert "mina" in history_text or "joon" in history_text


def test_plain_memory_text_still_works_when_relations_are_missing(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="graph-memory-fallback")
    graph_mem0.add(
        _message("Service Atlas uses Neo4j for graph search."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    payload = _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does Service Atlas use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["atlas", "uses", "neo4j"]],
        expected_memory_groups=[["atlas", "neo4j"]],
    )
    assert payload["relations"] or payload["memories"]


def test_relation_query_from_destination_side_is_retrievable(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="graph-destination")
    graph_mem0.add(
        _message("Project Atlas uses Neo4j."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="Which project uses Neo4j?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["atlas", "uses", "neo4j"]],
        expected_memory_groups=[["atlas", "neo4j"]],
    )


def test_mem0_restart_preserves_graph_recall(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    if not settings.run_optional or not settings.mem0_docker_container:
        pytest.skip("Set MEM0_RUN_OPTIONAL=1 and MEM0_DOCKER_CONTAINER to run Mem0 restart test.")

    scope = scope_factory(prefix="graph-mem0-restart")
    graph_mem0.add(
        _message("RestartSuite uses Neo4j."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does RestartSuite use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["restartsuite", "uses", "neo4j"]],
        expected_memory_groups=[["restartsuite", "neo4j"]],
    )

    _docker_restart(settings.mem0_docker_container)
    _wait_for_http_health(graph_mem0, settings)

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does RestartSuite use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["restartsuite", "uses", "neo4j"]],
        expected_memory_groups=[["restartsuite", "neo4j"]],
    )


def test_neo4j_restart_preserves_graph_recall(
    graph_mem0: GraphMem0Client,
    settings: Settings,
    scope_factory,
) -> None:
    if not settings.run_optional or not settings.neo4j_docker_container:
        pytest.skip("Set MEM0_RUN_OPTIONAL=1 and NEO4J_DOCKER_CONTAINER to run Neo4j restart test.")

    scope = scope_factory(prefix="graph-neo4j-restart")
    graph_mem0.add(
        _message("RestartGraphSuite uses Neo4j."),
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
    )
    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does RestartGraphSuite use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["restartgraphsuite", "uses", "neo4j"]],
        expected_memory_groups=[["restartgraphsuite", "neo4j"]],
    )

    _docker_restart(settings.neo4j_docker_container)
    _wait_for_http_health(graph_mem0, settings)

    _wait_for_graph_match(
        graph_mem0,
        settings,
        query="What does RestartGraphSuite use?",
        user_id=scope["user_id"],
        agent_id=scope["agent_id"],
        run_id=scope["run_id"],
        expected_relation_groups=[["restartgraphsuite", "uses", "neo4j"]],
        expected_memory_groups=[["restartgraphsuite", "neo4j"]],
    )
