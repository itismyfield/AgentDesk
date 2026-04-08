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
    default_limit: int = int(os.getenv("MEM0_DEFAULT_LIMIT", "5"))
    rerank: bool = _env_bool("MEM0_RERANK", False)
    run_integration: bool = _env_bool("MEM0_RUN_INTEGRATION", False)
    run_optional: bool = _env_bool("MEM0_RUN_OPTIONAL", False)
    expect_exclusion_prompt: bool = _env_bool("MEM0_EXPECT_EXCLUSION_PROMPT", False)
    mem0_docker_container: str | None = os.getenv("MEM0_DOCKER_CONTAINER") or None
    qdrant_docker_container: str | None = os.getenv("QDRANT_DOCKER_CONTAINER") or None


class Mem0RestClient:
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

    def healthcheck(self) -> None:
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
        filters: dict[str, Any] | None = None,
        infer: bool = True,
    ) -> Any:
        payload: dict[str, Any] = {
            "messages": messages,
            "user_id": user_id,
            "infer": infer,
        }
        if agent_id is not None:
            payload["agent_id"] = agent_id
        if run_id is not None:
            payload["run_id"] = run_id
        if metadata is not None:
            payload["metadata"] = metadata
        if filters is not None:
            payload["filters"] = filters
        return self._request("POST", "/memories", json=payload)

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
        }
        if agent_id is not None:
            payload["agent_id"] = agent_id
        if run_id is not None:
            payload["run_id"] = run_id
        if filters is not None:
            payload["filters"] = filters
        if rerank is None:
            rerank = self.settings.rerank
        payload["rerank"] = rerank
        return self._request("POST", "/search", json=payload)

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
        return self._request("GET", "/memories", params=params)

    def get_memory(self, memory_id: str) -> Any:
        return self._request("GET", f"/memories/{memory_id}")

    def update_memory(self, memory_id: str, *, text: str, metadata: dict[str, Any] | None = None) -> Any:
        payload: dict[str, Any] = {"text": text}
        if metadata is not None:
            payload["metadata"] = metadata
        return self._request("PUT", f"/memories/{memory_id}", json=payload)

    def delete_memory(self, memory_id: str) -> Any:
        return self._request("DELETE", f"/memories/{memory_id}")

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
            return self._request("DELETE", "/memories", params=params)
        except requests.HTTPError:
            payload = {k: v for k, v in params.items() if v is not None}
            return self._request("DELETE", "/memories", json=payload)

    def get_history(self, memory_id: str) -> Any:
        return self._request("GET", f"/memories/{memory_id}/history")


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


def _joined_top(payload: Any, limit: int = 5) -> str:
    return " || ".join(_norm(text) for text in _top_texts(payload, limit=limit))


def _matches_groups(payload: Any, expected_groups: list[list[str]], *, within: int = 3) -> bool:
    haystack = _joined_top(payload, limit=within)
    return any(all(_norm(token) in haystack for token in group) for group in expected_groups)


def _contains_forbidden(payload: Any, forbidden_tokens: Iterable[str], *, within: int = 5) -> bool:
    haystack = _joined_top(payload, limit=within)
    return any(_norm(token) in haystack for token in forbidden_tokens)


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


def _wait_for_search_match(
    mem0: Mem0RestClient,
    settings: Settings,
    *,
    query: str,
    user_id: str,
    agent_id: str | None = None,
    run_id: str | None = None,
    expected_groups: list[list[str]],
    within: int = 3,
    filters: dict[str, Any] | None = None,
) -> Any:
    def _probe():
        response = mem0.search(
            query,
            user_id=user_id,
            agent_id=agent_id,
            run_id=run_id,
            filters=filters,
        )
        return _matches_groups(response, expected_groups, within=within), _top_texts(response, limit=within)

    return _wait_until(
        _probe,
        timeout=settings.wait_timeout,
        poll=settings.wait_poll,
        on_timeout=(
            f"Expected query={query!r} to match {expected_groups} "
            f"for user_id={user_id}, agent_id={agent_id}, run_id={run_id}"
        ),
    )


def _wait_for_search_absence(
    mem0: Mem0RestClient,
    settings: Settings,
    *,
    query: str,
    user_id: str,
    forbidden_tokens: Iterable[str],
    agent_id: str | None = None,
    run_id: str | None = None,
    filters: dict[str, Any] | None = None,
) -> Any:
    def _probe():
        response = mem0.search(
            query,
            user_id=user_id,
            agent_id=agent_id,
            run_id=run_id,
            filters=filters,
        )
        return (not _contains_forbidden(response, forbidden_tokens), _top_texts(response))

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
    mem0: Mem0RestClient,
    settings: Settings,
    *,
    user_id: str,
    expected_count: int,
    agent_id: str | None = None,
    run_id: str | None = None,
) -> Any:
    def _probe():
        response = mem0.list_memories(user_id=user_id, agent_id=agent_id, run_id=run_id)
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


def _wait_for_http_health(mem0: Mem0RestClient, settings: Settings) -> None:
    def _probe():
        try:
            mem0.healthcheck()
            return True, "healthy"
        except Exception as exc:  # pragma: no cover - operational retry helper
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
def mem0(settings: Settings) -> Mem0RestClient:
    if not settings.run_integration:
        pytest.skip("Set MEM0_RUN_INTEGRATION=1 to run real Mem0/Qdrant integration tests.")
    client = Mem0RestClient(settings)
    client.healthcheck()
    return client


@pytest.fixture
def scope_factory(mem0: Mem0RestClient):
    created_user_ids: set[str] = set()

    def _make(
        *,
        prefix: str = "pytest",
        user_id: str | None = None,
        agent_id: str | None = None,
        run_id: str | None = None,
    ) -> dict[str, str]:
        base = f"{prefix}-{uuid.uuid4().hex[:10]}"
        uid = user_id or f"{base}-user"
        created_user_ids.add(uid)
        scope = {"user_id": uid}
        if agent_id is not None:
            scope["agent_id"] = agent_id
        if run_id is not None:
            scope["run_id"] = run_id
        return scope

    yield _make

    for user_id in created_user_ids:
        try:
            mem0.delete_scope(user_id=user_id)
        except Exception:
            pass


@dataclass(frozen=True)
class Ingestion:
    text: str
    metadata: dict[str, Any] = field(default_factory=dict)
    infer: bool = True


@dataclass(frozen=True)
class RecallScenario:
    slug: str
    query: str
    expected_groups: list[list[str]]
    ingestions: tuple[Ingestion, ...]
    description: str
    within: int = 3


SEMANTIC_SCENARIOS: tuple[RecallScenario, ...] = (
    RecallScenario(
        slug="aisle-seat-preference",
        description="Long-haul travel preference is captured and recalled.",
        query="What seat should I book on long flights?",
        expected_groups=[["aisle"]],
        ingestions=(Ingestion("For long-haul flights, I prefer aisle seats."),),
    ),
    RecallScenario(
        slug="coffee-avoidance",
        description="Avoidance constraints should be captured as durable facts.",
        query="What drink should I avoid?",
        expected_groups=[["coffee"]],
        ingestions=(Ingestion("Coffee gives me stomach pain, so please avoid recommending it."),),
    ),
    RecallScenario(
        slug="location-capture",
        description="Location from a profile statement is recalled.",
        query="Where do I live?",
        expected_groups=[["seoul"]],
        ingestions=(Ingestion("I live in Seoul and work remotely."),),
    ),
    RecallScenario(
        slug="profession-capture",
        description="Profession is recalled from profile data.",
        query="What do I do for work?",
        expected_groups=[["frontend", "engineer"], ["front-end", "engineer"]],
        ingestions=(Ingestion("I am a frontend engineer focused on TypeScript and UI systems."),),
    ),
    RecallScenario(
        slug="device-capture",
        description="Primary device is remembered.",
        query="What is my main work device?",
        expected_groups=[["macbook", "pro"], ["macbook"]],
        ingestions=(Ingestion("My main work laptop is a MacBook Pro."),),
    ),
    RecallScenario(
        slug="deadline-capture",
        description="A dated commitment is recalled.",
        query="What is my next deadline?",
        expected_groups=[["june", "15"], ["travel", "budget"]],
        ingestions=(Ingestion("I must submit the Tokyo travel budget by June 15."),),
    ),
    RecallScenario(
        slug="lactose-intolerance",
        description="Health restriction is captured for later retrieval.",
        query="Do I have any dairy restriction?",
        expected_groups=[["lactose"], ["dairy"]],
        ingestions=(Ingestion("I am lactose intolerant, so avoid dairy-heavy suggestions."),),
    ),
    RecallScenario(
        slug="indian-food-preference",
        description="Cuisine preference is recalled.",
        query="What cuisine do I enjoy?",
        expected_groups=[["indian"]],
        ingestions=(Ingestion("I love Indian food, especially spicy curries."),),
    ),
    RecallScenario(
        slug="api-route-convention",
        description="Project conventions should be searchable.",
        query="What API convention does project Phoenix use?",
        expected_groups=[["/api/v1"]],
        ingestions=(Ingestion("In project Phoenix, every API route follows /api/v1/{resource}."),),
    ),
    RecallScenario(
        slug="error-code-convention",
        description="Coding conventions around explicit errors are recalled.",
        query="What error-handling style does project Phoenix use?",
        expected_groups=[["explicit", "error", "code"], ["error", "codes"]],
        ingestions=(Ingestion("In project Phoenix, handlers must return explicit error codes."),),
    ),
    RecallScenario(
        slug="incident-root-cause",
        description="Incident root cause is remembered.",
        query="What caused the outage last Friday?",
        expected_groups=[["redis_url"], ["environment", "variable"]],
        ingestions=(Ingestion("Last Friday's outage was caused by a missing REDIS_URL environment variable."),),
    ),
    RecallScenario(
        slug="incident-fix",
        description="Incident resolution steps are remembered.",
        query="How was the outage fixed?",
        expected_groups=[["secrets"], ["redeploy"]],
        ingestions=(Ingestion("We fixed the outage by updating the secrets file and redeploying."),),
    ),
    RecallScenario(
        slug="science-fiction-paraphrase",
        description="Paraphrase recall should work for genre preference.",
        query="What movie genre do I like?",
        expected_groups=[["science", "fiction"], ["sci-fi"], ["sci", "fi"]],
        ingestions=(Ingestion("I enjoy science fiction movies, especially space exploration stories."),),
    ),
    RecallScenario(
        slug="disambiguation-latte-vs-caffeine",
        description="More specific recent constraint should win over generic preference.",
        query="What should I drink before a morning meeting?",
        expected_groups=[["avoid", "caffeine"], ["no", "caffeine"]],
        ingestions=(
            Ingestion("Usually I like caffe latte drinks."),
            Ingestion("Before morning meetings, avoid caffeine."),
        ),
    ),
    RecallScenario(
        slug="auth-state-summary",
        description="Current system state summary should be retrievable.",
        query="What is the current state of the auth module?",
        expected_groups=[["jwt"], ["refresh", "token"]],
        ingestions=(Ingestion("Current state: the auth module now uses JWT access tokens and refresh token rotation."),),
    ),
    RecallScenario(
        slug="avoid-thrillers",
        description="Negative preference should be recalled cleanly.",
        query="Which movie genre should I avoid?",
        expected_groups=[["thriller"]],
        ingestions=(Ingestion("I dislike thriller movies but I love sci-fi."),),
    ),
    RecallScenario(
        slug="dog-name",
        description="Small named entities should be searchable.",
        query="What is my dog's name?",
        expected_groups=[["miso"]],
        ingestions=(Ingestion("My dog's name is Miso."),),
    ),
)


@pytest.mark.integration
@pytest.mark.slow
@pytest.mark.parametrize("scenario", SEMANTIC_SCENARIOS, ids=[s.slug for s in SEMANTIC_SCENARIOS])
def test_capture_and_recall_semantic_scenarios(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
    scenario: RecallScenario,
) -> None:
    scope = scope_factory(prefix=scenario.slug)
    for ingestion in scenario.ingestions:
        mem0.add(
            _message(ingestion.text),
            user_id=scope["user_id"],
            metadata=ingestion.metadata or None,
            infer=ingestion.infer,
        )

    _wait_for_search_match(
        mem0,
        settings,
        query=scenario.query,
        user_id=scope["user_id"],
        expected_groups=scenario.expected_groups,
        within=scenario.within,
    )


@pytest.mark.integration
def test_dedup_with_infer_true_does_not_grow_count_on_duplicate_input(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="dedup")
    payload = _message("My preferred seat is aisle.")

    mem0.add(payload, user_id=scope["user_id"], infer=True)
    _wait_for_count(mem0, settings, user_id=scope["user_id"], expected_count=1)

    mem0.add(payload, user_id=scope["user_id"], infer=True)
    _wait_for_count(mem0, settings, user_id=scope["user_id"], expected_count=1)

    _wait_for_search_match(
        mem0,
        settings,
        query="What seat do I prefer?",
        user_id=scope["user_id"],
        expected_groups=[["aisle"]],
    )


@pytest.mark.integration
def test_user_scope_isolation(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    alice = scope_factory(prefix="user-scope-alice")
    bob = scope_factory(prefix="user-scope-bob")

    mem0.add(_message("Codename is ORION-HARBOR."), user_id=alice["user_id"], infer=False)
    mem0.add(_message("Codename is CEDAR-LAKE."), user_id=bob["user_id"], infer=False)

    response = mem0.search("What is my codename?", user_id=alice["user_id"])
    assert _matches_groups(response, [["orion-harbor"]], within=3)
    assert not _contains_forbidden(response, ["cedar-lake"], within=5)


@pytest.mark.integration
def test_agent_scope_isolation(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    shared_user = f"agent-scope-{uuid.uuid4().hex[:10]}"
    planner = scope_factory(user_id=shared_user, agent_id="planner", prefix="agent-planner")
    critic = scope_factory(user_id=shared_user, agent_id="critic", prefix="agent-critic")

    mem0.add(_message("Planning style is checklist-first."), user_id=planner["user_id"], agent_id=planner["agent_id"], infer=False)
    mem0.add(_message("Review style is nitpicky and adversarial."), user_id=critic["user_id"], agent_id=critic["agent_id"], infer=False)

    response = mem0.search(
        "What is my working style?",
        user_id=planner["user_id"],
        agent_id=planner["agent_id"],
    )
    assert _matches_groups(response, [["checklist"]], within=3)
    assert not _contains_forbidden(response, ["nitpicky", "adversarial"], within=5)


@pytest.mark.integration
def test_run_scope_isolation(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    shared_user = f"run-scope-{uuid.uuid4().hex[:10]}"
    shared_agent = "ops-bot"
    run_a = scope_factory(user_id=shared_user, agent_id=shared_agent, run_id="run-a", prefix="run-a")
    run_b = scope_factory(user_id=shared_user, agent_id=shared_agent, run_id="run-b", prefix="run-b")

    mem0.add(_message("Temporary room code is BLUE-17."), user_id=run_a["user_id"], agent_id=run_a["agent_id"], run_id=run_a["run_id"], infer=False)
    mem0.add(_message("Temporary room code is RED-44."), user_id=run_b["user_id"], agent_id=run_b["agent_id"], run_id=run_b["run_id"], infer=False)

    response = mem0.search(
        "What is the temporary room code?",
        user_id=run_a["user_id"],
        agent_id=run_a["agent_id"],
        run_id=run_a["run_id"],
    )
    assert _matches_groups(response, [["blue-17"]], within=3)
    assert not _contains_forbidden(response, ["red-44"], within=5)


@pytest.mark.integration
def test_metadata_exact_filter(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    scope = scope_factory(prefix="metadata-exact")
    mem0.add(
        _message("Work note: deploy via blue-green pipeline."),
        user_id=scope["user_id"],
        metadata={"category": "work", "source": "pytest"},
        infer=False,
    )
    mem0.add(
        _message("Personal note: buy hiking boots."),
        user_id=scope["user_id"],
        metadata={"category": "personal", "source": "pytest"},
        infer=False,
    )

    response = mem0.search(
        "What is the work note?",
        user_id=scope["user_id"],
        filters={"category": "work"},
    )
    assert _matches_groups(response, [["blue-green"], ["pipeline"]], within=3)
    assert not _contains_forbidden(response, ["hiking boots"], within=5)


@pytest.mark.integration
def test_metadata_logical_filters(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    scope = scope_factory(prefix="metadata-logic")
    mem0.add(
        _message("Prod sev1: rotate certificates immediately."),
        user_id=scope["user_id"],
        metadata={"category": "work", "priority": 9, "env": "prod"},
        infer=False,
    )
    mem0.add(
        _message("Dev task: clean lint warnings."),
        user_id=scope["user_id"],
        metadata={"category": "work", "priority": 5, "env": "dev"},
        infer=False,
    )
    mem0.add(
        _message("Staging smoke test checklist."),
        user_id=scope["user_id"],
        metadata={"category": "work", "priority": 8, "env": "staging"},
        infer=False,
    )

    response = mem0.search(
        "Which high-priority prod or staging work item should I focus on?",
        user_id=scope["user_id"],
        filters={
            "AND": [
                {"category": "work"},
                {"priority": {"gte": 7}},
                {"OR": [{"env": "prod"}, {"env": "staging"}]},
            ]
        },
    )
    assert not _contains_forbidden(response, ["lint warnings"], within=5)
    assert _matches_groups(
        response,
        [["rotate", "certificates"], ["staging", "smoke"]],
        within=3,
    )


@pytest.mark.integration
def test_update_latest_truth_wins(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    scope = scope_factory(prefix="update-latest")
    add_response = mem0.add(
        _message("Alarm time is 7:00 AM."),
        user_id=scope["user_id"],
        infer=False,
    )
    memory_id = _first_memory_id(add_response)
    assert memory_id, f"Could not extract memory id from add response: {add_response}"

    mem0.update_memory(memory_id, text="Alarm time is 6:30 AM.")

    response = mem0.search("What time is the alarm?", user_id=scope["user_id"])
    top = _joined_top(response, limit=1)
    assert "6:30" in top
    assert "7:00" not in top


@pytest.mark.integration
def test_delete_removes_memory_from_recall(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    scope = scope_factory(prefix="delete-recall")
    add_response = mem0.add(
        _message("Passport locker code is AZURE-BADGER-17."),
        user_id=scope["user_id"],
        infer=False,
    )
    memory_id = _first_memory_id(add_response)
    assert memory_id, f"Could not extract memory id from add response: {add_response}"

    _wait_for_search_match(
        mem0,
        settings,
        query="What is the passport locker code?",
        user_id=scope["user_id"],
        expected_groups=[["azure-badger-17"]],
    )

    mem0.delete_memory(memory_id)

    _wait_for_search_absence(
        mem0,
        settings,
        query="What is the passport locker code?",
        user_id=scope["user_id"],
        forbidden_tokens=["azure-badger-17"],
    )


@pytest.mark.integration
def test_history_exists_after_update(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    scope = scope_factory(prefix="history")
    add_response = mem0.add(
        _message("Preferred IDE is VS Code."),
        user_id=scope["user_id"],
        infer=False,
    )
    memory_id = _first_memory_id(add_response)
    assert memory_id, f"Could not extract memory id from add response: {add_response}"

    mem0.update_memory(memory_id, text="Preferred IDE is Neovim.")

    history = mem0.get_history(memory_id)
    items = _result_items(history)
    dump = json.dumps(history, ensure_ascii=False).lower()
    assert items or "history" in dump or "update" in dump
    assert "vscode" in dump or "neovim" in dump or "vs code" in dump


@pytest.mark.integration
def test_new_client_object_can_recall_existing_user_memory(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
) -> None:
    scope = scope_factory(prefix="new-client-recall")
    mem0.add(
        _message("The preferred editor theme is Tokyo Night."),
        user_id=scope["user_id"],
        infer=False,
    )

    _wait_for_search_match(
        mem0,
        settings,
        query="What editor theme do I prefer?",
        user_id=scope["user_id"],
        expected_groups=[["tokyo", "night"]],
    )

    fresh_client = Mem0RestClient(settings)
    response = fresh_client.search(
        "What editor theme do I prefer?",
        user_id=scope["user_id"],
    )
    assert _matches_groups(response, [["tokyo", "night"]], within=3)


@pytest.mark.integration
@pytest.mark.optional
def test_qdrant_restart_persists_memory(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
) -> None:
    if not settings.run_optional or not settings.qdrant_docker_container:
        pytest.skip("Set MEM0_RUN_OPTIONAL=1 and QDRANT_DOCKER_CONTAINER to run restart tests.")

    scope = scope_factory(prefix="qdrant-restart")
    mem0.add(
        _message("The monitoring dashboard alias is OBSIDIAN-SIGNAL."),
        user_id=scope["user_id"],
        infer=False,
    )
    _wait_for_search_match(
        mem0,
        settings,
        query="What is the monitoring dashboard alias?",
        user_id=scope["user_id"],
        expected_groups=[["obsidian-signal"]],
    )

    _docker_restart(settings.qdrant_docker_container)
    _wait_for_http_health(mem0, settings)

    response = mem0.search("What is the monitoring dashboard alias?", user_id=scope["user_id"])
    assert _matches_groups(response, [["obsidian-signal"]], within=3)


@pytest.mark.integration
@pytest.mark.optional
def test_mem0_restart_persists_memory(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
) -> None:
    if not settings.run_optional or not settings.mem0_docker_container:
        pytest.skip("Set MEM0_RUN_OPTIONAL=1 and MEM0_DOCKER_CONTAINER to run restart tests.")

    scope = scope_factory(prefix="mem0-restart")
    mem0.add(
        _message("My shell prompt style is minimal powerline."),
        user_id=scope["user_id"],
        infer=False,
    )
    _wait_for_search_match(
        mem0,
        settings,
        query="What shell prompt style do I use?",
        user_id=scope["user_id"],
        expected_groups=[["minimal", "powerline"]],
    )

    _docker_restart(settings.mem0_docker_container)
    _wait_for_http_health(mem0, settings)

    response = mem0.search("What shell prompt style do I use?", user_id=scope["user_id"])
    assert _matches_groups(response, [["minimal", "powerline"]], within=3)


@pytest.mark.integration
@pytest.mark.optional
def test_multilingual_recall(mem0: Mem0RestClient, settings: Settings, scope_factory) -> None:
    if not settings.run_optional:
        pytest.skip("Set MEM0_RUN_OPTIONAL=1 to run optional multilingual test.")

    scope = scope_factory(prefix="multilingual")
    mem0.add(
        _message("나는 매운 음식을 좋아하고 차를 즐겨 마셔."),
        user_id=scope["user_id"],
        infer=True,
    )

    response = mem0.search("What kind of food do I like?", user_id=scope["user_id"])
    assert _matches_groups(response, [["매운"], ["spicy"]], within=3)


@pytest.mark.integration
@pytest.mark.optional
def test_exclusion_prompt_blocks_secret_but_keeps_timezone(
    mem0: Mem0RestClient,
    settings: Settings,
    scope_factory,
) -> None:
    if not settings.run_optional or not settings.expect_exclusion_prompt:
        pytest.skip(
            "Requires MEM0_RUN_OPTIONAL=1 and MEM0_EXPECT_EXCLUSION_PROMPT=1 "
            "with a custom fact extraction prompt configured on the server."
        )

    scope = scope_factory(prefix="exclusion")
    mem0.add(
        _message("The API key is sk-secret-123. My timezone is KST. Nice weather today."),
        user_id=scope["user_id"],
        infer=True,
    )

    timezone_response = mem0.search("What is my timezone?", user_id=scope["user_id"])
    assert _matches_groups(timezone_response, [["kst"]], within=3)

    secret_response = mem0.search("What is my API key?", user_id=scope["user_id"])
    assert not _contains_forbidden(secret_response, ["sk-secret-123"], within=5)
