<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import {
    McpHttpClient,
    extractToolPayload,
    isRecord,
    type ToolDescriptor,
  } from "../lib/mcp";

  export let repoPath: string;
  export let onNavigate: (path: string) => void;

  // --------------------------------------------------------------------------
  // Graph types
  // --------------------------------------------------------------------------

  type GNode = {
    id: string;
    kind: string;
    label: string;
    detail?: string | null;
    x?: number;
    y?: number;
    z?: number;
  };
  type GLink = {
    id: string;
    source: string | GNode;
    target: string | GNode;
    edge_type: string;
    confidence: number;
    cross_file: boolean;
  };

  // --------------------------------------------------------------------------
  // Semantic color palette
  // --------------------------------------------------------------------------

  const KIND_COLORS: Record<string, string> = {
    decision: "#f5a623", // amber
    lead: "#d4a0f7", // soft purple — observed patterns without a formal ADR
    file: "#6ee7ff", // cyan
    symbol: "#89f0a7", // green
    commit: "#8fa8c8", // muted blue
    constraint: "#f4845f", // coral
  };

  const EDGE_COLORS: Record<string, string> = {
    calls: "#6ee7ff",
    imports: "#b7fff5",
    contains: "rgba(255,255,255,0.12)",
    imposes: "#f4845f",
    mentions: "#d4a0f7", // soft purple — lead/decision → file links
    links_to: "#f5a623",
    references: "#89f0a7",
    references_commit: "#8fa8c8",
    modifies: "#8fa8c8",
    implements: "#c8a6f5",
    extends: "#e0c0ff",
    inherits: "#e0c0ff",
    uses: "#a0d0ff",
    uses_type: "#d4b8f5", // soft lavender — type-signature coupling
    depends_on: "#4ecdc4", // teal — file-level structural dependency
    co_changes_with: "#ffd166", // warm yellow — historical commit coupling
  };

  const PROJECTED_SYMBOL_FILE_EDGE_PREFIX = "ui-symbol-file";

  function nodeColor(n: object): string {
    const node = n as GNode;
    if (selectedNode && !isSelectedNeighborhoodNode(node.id)) {
      return "rgba(143,168,200,0.28)";
    }
    if (selectedNode?.id === node.id) return "#ffffff";
    return KIND_COLORS[node.kind] ?? "#aaaaaa";
  }

  function edgeColor(l: object): string {
    const link = l as GLink;
    if (selectedNode && !linkTouchesNode(link, selectedNode.id)) {
      return "rgba(143,168,200,0.16)";
    }
    const base = EDGE_COLORS[link.edge_type] ?? "rgba(255,255,255,0.18)";
    // Cross-file edges rendered brighter
    if (link.cross_file && link.edge_type === "calls") return "#ffffff";
    return base;
  }

  function linkVisible(l: object): boolean {
    const link = l as GLink;
    return visibleEdgeTypes[link.edge_type] ?? false;
  }

  function linkOpacity(l: object): number {
    const link = l as GLink;
    if (!selectedNode) return 0.7;
    return linkTouchesNode(link, selectedNode.id) ? 0.9 : 0.12;
  }

  function linkParticleCount(l: object): number {
    const link = l as GLink;
    if (!linkVisible(link) || isProjectedFileLink(link)) return 0;
    const visibleTypeCount = edgeCounts[link.edge_type] ?? 0;
    if (visibleTypeCount > 250) return 0;
    return link.cross_file ? 2 : 1;
  }

  function linkEndpointId(endpoint: string | GNode): string {
    return typeof endpoint === "string" ? endpoint : endpoint.id;
  }

  function isProjectedFileLink(link: GLink): boolean {
    return (
      link.edge_type === "contains" &&
      String(link.id).startsWith(`${PROJECTED_SYMBOL_FILE_EDGE_PREFIX}:`) &&
      linkEndpointId(link.source).startsWith("file:")
    );
  }

  function linkTouchesNode(link: GLink, nodeId: string): boolean {
    return linkEndpointId(link.source) === nodeId || linkEndpointId(link.target) === nodeId;
  }

  function otherLinkEndpoint(link: GLink, nodeId: string): string {
    const source = linkEndpointId(link.source);
    const target = linkEndpointId(link.target);
    return source === nodeId ? target : source;
  }

  function isSelectedNeighborhoodNode(nodeId: string): boolean {
    if (!selectedNode) return true;
    if (selectedNode.id === nodeId) return true;
    return graph.links.some((link) => linkTouchesNode(link, selectedNode!.id) && otherLinkEndpoint(link, selectedNode!.id) === nodeId);
  }

  function graphLinkDistance(link: GLink): number {
    if (isProjectedFileLink(link)) return closenessToDistance(fileCloseness, 55, 360);
    const distance = closenessToDistance(edgeCloseness[link.edge_type] ?? 42, 90, 900);
    return link.cross_file ? distance * 1.2 : distance;
  }

  function graphLinkStrength(link: GLink): number {
    if (isProjectedFileLink(link)) return closenessToStrength(fileCloseness, 0.08, 2.0);
    const strength = closenessToStrength(edgeCloseness[link.edge_type] ?? 42, 0.04, 1.25);
    return link.cross_file ? strength * 0.55 : strength;
  }

  function graphNodeCharge(node: object): number {
    const kind = (node as GNode).kind;
    if (kind === "file") return -250 - fileSpacing * 12;
    if (kind === "symbol") return -90;
    return -180;
  }

  function closenessToDistance(value: number, min: number, max: number): number {
    const clamped = Math.max(0, Math.min(100, value));
    return max - ((max - min) * clamped) / 100;
  }

  function closenessToStrength(value: number, min: number, max: number): number {
    const clamped = Math.max(0, Math.min(100, value));
    return min + ((max - min) * clamped) / 100;
  }

  function applyGraphForces() {
    if (!graphApi || graph.nodes.length === 0) return;
    graphApi
      .d3Force("link")
      ?.distance((l: object) => graphLinkDistance(l as GLink))
      ?.strength((l: object) => graphLinkStrength(l as GLink));
    graphApi.d3Force("charge")?.strength(graphNodeCharge);
    graphApi.d3ReheatSimulation?.();
  }

  function applyGraphRendering() {
    if (!graphApi) return;
    graphApi.linkVisibility(linkVisible);
    graphApi.nodeColor(nodeColor);
    graphApi.linkColor(edgeColor);
    graphApi.linkOpacity(linkOpacity);
    graphApi.linkDirectionalParticles(linkParticleCount);
    graphApi.nodeOpacity?.(1);
  }

  // --------------------------------------------------------------------------
  // State
  // --------------------------------------------------------------------------

  let client: McpHttpClient | null = null;
  let status = "Disconnected";
  let errorMsg: string | null = null;

  let tools: ToolDescriptor[] = [];
  let toolSearch = "";
  let toolCategoryFilter = "all";
  let navigatorTab: "entities" | "tools" = "entities";
  let entitySearch = "";
  let entityKindFilter = "all";
  let selectedTool = "";
  let toolInput = "";
  let resultMode: "summary" | "json" = "summary";
  let running = false;

  let graph: { nodes: GNode[]; links: GLink[] } = { nodes: [], links: [] };
  let selectedNode: GNode | null = null;
  let retractReason = "";
  let retractRunning = false;
  let retractError: string | null = null;

  let graphEl: HTMLDivElement;
  let graphApi: any = null;

  // Graph name filter
  let nodeFilter = "";
  let snapshotMode: "full" | "sampled" = "full";
  let snapshotMaxNodes = 80;
  let snapshotMaxEdges = 800;
  let fileCloseness = 82;
  let fileSpacing = 70;
  let showLayoutControls = false;
  let visibleNodeKinds: Record<string, boolean> = Object.fromEntries(
    Object.keys(KIND_COLORS).map((kind) => [kind, true]),
  );
  let visibleEdgeTypes: Record<string, boolean> = Object.fromEntries(
    Object.keys(EDGE_COLORS).map((type) => [type, false]),
  );
  let edgeCloseness: Record<string, number> = {
    calls: 45,
    imports: 42,
    contains: 68,
    imposes: 38,
    links_to: 38,
    references: 38,
    references_commit: 34,
    modifies: 34,
    implements: 48,
    extends: 48,
    inherits: 48,
    uses: 44,
    uses_type: 54,
    depends_on: 42,
    co_changes_with: 32,
  };

  // Preserved node objects — same references survive updates so x/y/z (physics
  // positions set by the simulation) are not lost on incremental data changes.
  const nodeMap = new Map<string, GNode>();

  // Live graph refresh during active ingest/sync
  let graphRefreshTimer: ReturnType<typeof setInterval> | null = null;

  // Server notification listener cleanup
  let stopListening: (() => void) | null = null;

  // --------------------------------------------------------------------------
  // Form mode state
  // --------------------------------------------------------------------------

  type FormField = {
    key: string;
    type: string;
    description?: string;
    required: boolean;
    enum?: string[];
    multiline: boolean;
  };

  type ToolResultRecord = {
    toolName: string;
    ranAt: string;
    status: "success" | "error";
    payload: unknown;
    text: string;
  };

  type SummaryMetric = {
    label: string;
    value: string | number;
  };

  type SummaryList = {
    title: string;
    items: string[];
  };

  type WorkspaceTab =
    | "overview"
    | "relationships"
    | "history"
    | "docs"
    | "graph"
    | "advanced";

  type RelationshipRow = {
    id: string;
    edge: GLink;
    direction: "in" | "out";
    other: GNode | null;
    otherId: string;
    relation: string;
    sourceLabel: string;
  };

  type EntityGroup =
    | "all"
    | "high_impact"
    | "recent"
    | "entry_points"
    | "tests"
    | "generated_leads";

  type EntitySort = "relevance" | "relationships" | "name" | "kind" | "recent";

  const WORKSPACE_VIEW_TABS: { key: WorkspaceTab; label: string }[] = [
    { key: "graph", label: "Visual graph" },
  ];

  const ENTITY_WORKSPACE_TABS: { key: WorkspaceTab; label: string }[] = [
    { key: "overview", label: "Overview" },
    { key: "relationships", label: "Relationships" },
    { key: "history", label: "History" },
    { key: "docs", label: "Docs & Claims" },
  ];

  let jsonMode = false;
  let formValues: Record<string, unknown> = {};
  let toolResults: Record<string, ToolResultRecord> = {};
  let workspaceTab: WorkspaceTab = "graph";
  let selectedRelationshipId: string | null = null;
  let entityGroupFilter: EntityGroup = "all";
  let entitySort: EntitySort = "relevance";

  // Ingest status tracking
  let ingestJobId: string | null = null;
  let ingestState: "idle" | "running" | "done" | "error" | "cancelled" = "idle";
  let ingestMsg = "";
  let ingestPollTimer: ReturnType<typeof setInterval> | null = null;

  // Leads synthesis tracking
  let leadsJobId: string | null = null;
  let leadsState: "idle" | "running" | "done" | "error" = "idle";
  let leadsMsg = "";
  let leadsPollTimer: ReturnType<typeof setInterval> | null = null;

  $: toolMatches = tools.filter((t) => {
    const hay = `${t.name} ${t.description ?? ""}`.toLowerCase();
    return hay.includes(toolSearch.toLowerCase());
  });

  $: filteredTools = toolMatches
    .filter((tool) => toolCategoryFilter === "all" || toolCategory(tool) === toolCategoryFilter)
    .sort((a, b) => {
      const categoryDelta = toolCategory(a).localeCompare(toolCategory(b));
      if (categoryDelta !== 0) return categoryDelta;
      return a.name.localeCompare(b.name);
    });

  $: toolCategorySummaries = toolCategoryNames()
    .map((category) => ({
      category,
      count: toolMatches.filter((tool) => toolCategory(tool) === category).length,
      total: tools.filter((tool) => toolCategory(tool) === category).length,
    }))
    .filter((summary) => summary.total > 0);

  $: searchGraph = (() => {
    if (!nodeFilter.trim()) return graph;
    const q = nodeFilter.toLowerCase();
    const matchIds = new Set(
      graph.nodes
        .filter(
          (n) =>
            n.label.toLowerCase().includes(q) || n.id.toLowerCase().includes(q),
        )
        .map((n) => n.id),
    );
    // Include 1-hop neighbours of matched nodes
    for (const l of graph.links) {
      const src =
        typeof l.source === "string" ? l.source : (l.source as GNode).id;
      const tgt =
        typeof l.target === "string" ? l.target : (l.target as GNode).id;
      if (matchIds.has(src)) matchIds.add(tgt);
      if (matchIds.has(tgt)) matchIds.add(src);
    }
    const nodes = graph.nodes.filter((n) => matchIds.has(n.id));
    const links = graph.links.filter((l) => {
      const src =
        typeof l.source === "string" ? l.source : (l.source as GNode).id;
      const tgt =
        typeof l.target === "string" ? l.target : (l.target as GNode).id;
      return matchIds.has(src) && matchIds.has(tgt);
    });
    return { nodes, links };
  })();

  $: displayGraph = (() => {
    const nodes = searchGraph.nodes.filter(
      (node) => visibleNodeKinds[node.kind] ?? true,
    );
    const nodeIds = new Set(nodes.map((node) => node.id));
    const links = searchGraph.links.filter((link) => {
      const source = linkEndpointId(link.source);
      const target = linkEndpointId(link.target);
      return nodeIds.has(source) && nodeIds.has(target);
    });
    return { nodes, links };
  })();

  $: if (graphApi && displayGraph) {
    graphApi.graphData({
      nodes: displayGraph.nodes,
      links: displayGraph.links,
    });
    applyGraphRendering();
  }

  $: nodeCounts = searchGraph.nodes.reduce(
    (counts, node) => {
      counts[node.kind] = (counts[node.kind] ?? 0) + 1;
      return counts;
    },
    {} as Record<string, number>,
  );

  $: edgeCounts = displayGraph.links.reduce(
    (counts, link) => {
      counts[link.edge_type] = (counts[link.edge_type] ?? 0) + 1;
      if (link.cross_file && link.edge_type === "calls") {
        counts.cross_file_calls = (counts.cross_file_calls ?? 0) + 1;
      }
      return counts;
    },
    {} as Record<string, number>,
  );

  $: relationshipCounts = graph.links.reduce((counts, link) => {
    const source = linkEndpointId(link.source);
    const target = linkEndpointId(link.target);
    counts[source] = (counts[source] ?? 0) + 1;
    counts[target] = (counts[target] ?? 0) + 1;
    return counts;
  }, {} as Record<string, number>);

  $: entityKindOptions = Object.keys(
    displayGraph.nodes.reduce(
      (counts, node) => {
        counts[node.kind] = true;
        return counts;
      },
      {} as Record<string, boolean>,
    ),
  ).sort();

  $: entityMatches = displayGraph.nodes
    .filter((node) => {
      const query = entitySearch.trim().toLowerCase();
      if (!query) return true;
      return entitySearchText(node).toLowerCase().includes(query);
    });

  $: entityGroupSummaries = entityGroupOptions().map((group) => ({
    ...group,
    matching: entityMatches.filter((node) => entityMatchesGroup(node, group.key)).length,
  }));

  $: entityRows = entityMatches
    .filter((node) => entityKindFilter === "all" || node.kind === entityKindFilter)
    .filter((node) => entityMatchesGroup(node, entityGroupFilter))
    .sort(compareEntities);

  $: entityKindSummaries = entityKindOptions
    .map((kind) => {
      const matching = entityMatches.filter((node) => node.kind === kind).length;
      return {
        kind,
        matching,
        total: nodeCounts[kind] ?? 0,
      };
    })
    .sort((a, b) => {
      const countDelta = b.total - a.total;
      if (countDelta !== 0) return countDelta;
      return a.kind.localeCompare(b.kind);
    });

  $: selectedEdges = selectedNode
    ? graph.links.filter((link) => linkTouchesNode(link, selectedNode!.id))
    : [];

  $: selectedRelationRows = selectedNode
    ? selectedEdges
        .map((link): RelationshipRow => {
          const source = linkEndpointId(link.source);
          const otherId = otherLinkEndpoint(link, selectedNode!.id);
          return {
            id: link.id,
            edge: link,
            direction: source === selectedNode!.id ? "out" : "in",
            other: nodeMap.get(otherId) ?? graph.nodes.find((node) => node.id === otherId) ?? null,
            otherId,
            relation: link.edge_type,
            sourceLabel: relationshipSourceLabel(link),
          };
        })
        .sort((a, b) => {
          const relDelta = a.relation.localeCompare(b.relation);
          if (relDelta !== 0) return relDelta;
          return (a.other?.label ?? a.otherId).localeCompare(b.other?.label ?? b.otherId);
        })
    : [];

  $: selectedRelationship = selectedRelationRows.find((row) => row.id === selectedRelationshipId) ?? selectedRelationRows[0] ?? null;

  $: selectedRelationTypeCounts = selectedRelationRows.reduce(
    (counts, row) => {
      counts[row.relation] = (counts[row.relation] ?? 0) + 1;
      return counts;
    },
    {} as Record<string, number>,
  );

  $: selectedDocsAndClaims = selectedRelationRows.filter((row) => {
    const kind = row.other?.kind;
    return kind === "decision" || kind === "lead" || kind === "constraint" || row.relation.includes("mention") || row.relation.includes("reference");
  });

  $: selectedHistoryRows = selectedRelationRows.filter((row) => row.other?.kind === "commit" || row.relation.includes("modif") || row.relation.includes("co_change"));

  $: visibleEdgeCount = displayGraph.links.filter(
    (link) => visibleEdgeTypes[link.edge_type] ?? false,
  ).length;

  $: currentTool = tools.find((t) => t.name === selectedTool) ?? null;
  $: currentToolResult = selectedTool ? toolResults[selectedTool] ?? null : null;
  $: currentResultMetrics = currentToolResult
    ? resultMetrics(currentToolResult.toolName, currentToolResult.payload)
    : [];
  $: currentResultLists = currentToolResult
    ? resultLists(currentToolResult.toolName, currentToolResult.payload)
    : [];

  $: formFields = (() => {
    if (!currentTool) return [] as FormField[];
    const schema = currentTool.inputSchema;
    const required: string[] = Array.isArray(schema?.required)
      ? (schema.required as string[])
      : [];
    const props = isRecord(schema?.properties)
      ? (schema.properties as Record<string, unknown>)
      : {};
    return Object.entries(props).map(([key, prop]): FormField => {
      const p = isRecord(prop) ? prop : {};
      return {
        key,
        type: schemaType(p),
        description: p.description ? String(p.description) : undefined,
        required: required.includes(key),
        enum: Array.isArray(p.enum)
          ? (p.enum as unknown[]).map(String)
          : undefined,
        multiline: [
          "query",
          "text",
          "content",
          "description",
          "patch",
          "body",
        ].includes(key),
      };
    });
  })();

  // --------------------------------------------------------------------------
  // Lifecycle
  // --------------------------------------------------------------------------

  onMount(async () => {
    const endpoint = sessionStorage.getItem("weaver.endpoint") ?? "/mcp";

    try {
      status = "Connecting…";
      const next = new McpHttpClient(endpoint);
      await next.initialize("weaver-manager");
      client = next;

      tools = (await client.listTools()).sort((a, b) =>
        a.name.localeCompare(b.name),
      );
      status = `Connected · ${tools.length} tools`;

      stopListening = client.listen((method) => {
        if (method === "notifications/tools/list_changed") {
          client
            ?.listTools()
            .then((t) => {
              tools = t.sort((a, b) => a.name.localeCompare(b.name));
              status = `Connected · ${tools.length} tools`;
            })
            .catch(() => {});
        }
      });

    } catch (err) {
      errorMsg = toStr(err);
      status = "Connection failed";
    }

    try {
      const { default: ForceGraph3D } = await import("3d-force-graph");
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const THREE = (await import("three")) as any;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const { UnrealBloomPass } = (await import(
        "three/examples/jsm/postprocessing/UnrealBloomPass.js"
      )) as any;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const { SMAAPass } = (await import(
        "three/examples/jsm/postprocessing/SMAAPass.js"
      )) as any;

      graphApi = (ForceGraph3D as unknown as () => any)()(graphEl)
        .backgroundColor("#07111b")
        .nodeColor(nodeColor)
        .nodeRelSize(10)
        .nodeOpacity(1)
        .nodeLabel((n: object) => {
          const node = n as GNode;
          return `<div style="font:13px/1.5 monospace;padding:4px 8px;background:rgba(7,17,27,0.88);border-radius:8px;border:1px solid rgba(110,231,255,0.25)">
            <b style="color:${KIND_COLORS[node.kind] ?? "#fff"}">${node.label}</b><br>
            <span style="color:#8fa8c8;font-size:11px">${node.kind}${node.detail ? ` · ${node.detail}` : ""}</span>
          </div>`;
        })
        .linkColor(edgeColor)
        .linkVisibility(linkVisible)
        .linkLabel((l: object) => {
          const link = l as GLink;
          return `${link.edge_type}${link.cross_file ? " (cross-file)" : ""} · conf ${link.confidence.toFixed(2)}`;
        })
        .linkWidth((l: object) => {
          const link = l as GLink;
          if (isProjectedFileLink(link)) return 0.35;
          return link.cross_file ? 0.7 : 1;
        })
        .linkOpacity(linkOpacity)
        .linkDirectionalParticles(linkParticleCount)
        .linkDirectionalParticleSpeed(0.004)
        .linkDirectionalParticleColor(edgeColor)
        .onNodeClick((n: object) => {
          selectNode(n as GNode);
        });

      // Intra-file edges cluster tighter; cross-file edges spread out
      graphApi
        .d3Force("link")
        ?.distance((l: object) => graphLinkDistance(l as GLink))
        ?.strength((l: object) => graphLinkStrength(l as GLink));

      const { width, height } = graphEl.getBoundingClientRect();
      const w = width || 800;
      const h = height || 480;
      const bloomPass = new UnrealBloomPass(
        new THREE.Vector2(w, h),
        0.5,
        0,
        0.35,
      );
      graphApi.postProcessingComposer().addPass(bloomPass);
      const smaaPass = new SMAAPass(w, h);
      graphApi.postProcessingComposer().addPass(smaaPass);

      await loadGraph();
      resizeGraph();
      applyGraphForces();
      window.addEventListener("resize", resizeGraph);
    } catch (err) {
      console.error("3D graph init failed:", err);
    }
  });

  onDestroy(() => {
    client?.disconnect();
    graphApi?._destructor?.();
    stopIngestPoll();
    stopLeadsPoll();
    stopGraphRefresh();
    stopListening?.();
    window.removeEventListener("resize", resizeGraph);
  });

  // --------------------------------------------------------------------------
  // Tool actions
  // --------------------------------------------------------------------------

  function selectTool(tool: ToolDescriptor) {
    selectedTool = tool.name;
    toolInput = buildTemplate(tool);
    jsonMode = false;
    formValues = initFormValues(tool);
    navigatorTab = "tools";
    workspaceTab = "advanced";
  }

  function selectNode(node: GNode, center = true) {
    selectedNode = node;
    selectedRelationshipId = null;
    retractReason = "";
    retractError = null;
    if (center) {
      graphApi?.centerAt(node.x ?? 0, node.y ?? 0, 1200);
      graphApi?.zoom(6, 800);
    }
    applyGraphRendering();
  }

  function selectNavigatorTab(tab: "entities" | "tools") {
    navigatorTab = tab;
    if (tab === "tools") {
      workspaceTab = "advanced";
      return;
    }
    if (workspaceTab === "advanced") {
      workspaceTab = selectedNode ? "overview" : "graph";
    }
  }

  function entitySearchText(node: GNode): string {
    return `${node.kind} ${node.label} ${node.detail ?? ""} ${node.id}`;
  }

  function entityTitle(node: GNode): string {
    return [node.label, node.detail, node.id].filter(Boolean).join("\n");
  }

  function toolCategoryNames(): string[] {
    return ["Explore", "Search", "Context", "Sync", "Governance", "Admin"];
  }

  function toolCategory(tool: ToolDescriptor | string): string {
    const name = typeof tool === "string" ? tool : tool.name;
    if (name.includes("graph") || name.includes("architecture") || name.includes("diff")) return "Explore";
    if (name.includes("query") || name.includes("find") || name.includes("explain")) return "Search";
    if (name.includes("brief") || name.includes("trace") || name.includes("impact") || name.includes("orphan") || name.includes("stale")) return "Context";
    if (name.includes("ingest") || name.includes("sync") || name.includes("embed") || name.includes("cancel")) return "Sync";
    if (name.includes("adr") || name.includes("decision") || name.includes("constraint") || name.includes("retract") || name.includes("lead")) return "Governance";
    return "Admin";
  }

  function entityGroupOptions(): { key: EntityGroup; label: string }[] {
    return [
      { key: "all", label: "All" },
      { key: "high_impact", label: "High impact" },
      { key: "recent", label: "Recently changed" },
      { key: "entry_points", label: "Entry points" },
      { key: "tests", label: "Tests" },
      { key: "generated_leads", label: "Generated leads" },
    ];
  }

  function entityMatchesGroup(node: GNode, group: EntityGroup): boolean {
    if (group === "all") return true;
    const text = entitySearchText(node).toLowerCase();
    if (group === "high_impact") return (relationshipCounts[node.id] ?? 0) >= 10;
    if (group === "recent") return node.kind === "commit" || graph.links.some((link) => linkTouchesNode(link, node.id) && (link.edge_type.includes("modif") || link.edge_type.includes("co_change")));
    if (group === "entry_points") return /(^|[/\\])(main|server|routes?|cli|index)\./i.test(text) || /\b(main|server|route|handler|entry)\b/i.test(text);
    if (group === "tests") return /(^|[/\\])tests?[/\\]|_test\.|\.test\.|\.spec\./i.test(text);
    return node.kind === "lead";
  }

  function compareEntities(a: GNode, b: GNode): number {
    if (entitySort === "name") return a.label.localeCompare(b.label);
    if (entitySort === "kind") {
      const kindDelta = a.kind.localeCompare(b.kind);
      return kindDelta !== 0 ? kindDelta : a.label.localeCompare(b.label);
    }
    if (entitySort === "recent") {
      const recentDelta = entityRecentScore(b) - entityRecentScore(a);
      if (recentDelta !== 0) return recentDelta;
    }
    const relDelta = (relationshipCounts[b.id] ?? 0) - (relationshipCounts[a.id] ?? 0);
    if (relDelta !== 0) return relDelta;
    return a.label.localeCompare(b.label);
  }

  function entityRecentScore(node: GNode): number {
    if (node.kind === "commit") return 1000 + (relationshipCounts[node.id] ?? 0);
    return graph.links.filter((link) => linkTouchesNode(link, node.id) && (link.edge_type.includes("modif") || link.edge_type.includes("co_change"))).length;
  }

  function entityDetail(node: GNode): string {
    return node.detail || node.id;
  }

  function entityPath(node: GNode): string | null {
    return selectedFilePath(node);
  }

  function relationshipSourceLabel(link: GLink): string {
    const parts = [];
    if (isProjectedFileLink(link)) parts.push("UI projection");
    else if (link.cross_file) parts.push("cross-file");
    else parts.push("graph");
    parts.push(`confidence ${link.confidence.toFixed(2)}`);
    return parts.join(" · ");
  }

  function selectWorkspaceTab(tab: WorkspaceTab) {
    if (!selectedNode && ENTITY_WORKSPACE_TABS.some((item) => item.key === tab)) {
      workspaceTab = "graph";
      navigatorTab = "entities";
      return;
    }
    workspaceTab = tab;
    navigatorTab = tab === "advanced" ? "tools" : "entities";
    if (tab === "graph") {
      setTimeout(resizeGraph, 0);
    }
  }

  function resizeGraph() {
    if (!graphApi || !graphEl) return;
    const { width, height } = graphEl.getBoundingClientRect();
    if (width > 0) graphApi.width(width);
    if (height > 0) graphApi.height(height);
  }

  function resultMetrics(toolName: string, payload: unknown): SummaryMetric[] {
    if (!isRecord(payload)) {
      return [{ label: "Result", value: typeof payload }];
    }

    if (toolName === "get_graph_snapshot" && Array.isArray(payload.nodes) && Array.isArray(payload.edges)) {
      const nodes = payload.nodes.filter(isRecord);
      const edges = payload.edges.filter(isRecord);
      const nodeKinds = countBy(nodes, (node) => String(node.kind ?? "unknown"));
      const edgeTypes = countBy(edges, (edge) => String(edge.edge_type ?? "related"));
      return [
        { label: "Nodes", value: nodes.length },
        { label: "Edges", value: edges.length },
        { label: "Kinds", value: Object.keys(nodeKinds).length },
        { label: "Line types", value: Object.keys(edgeTypes).length },
      ];
    }

    if (toolName === "index_status" && Array.isArray(payload.entities)) {
      const entities = payload.entities.filter(isRecord);
      const total = sumNumeric(entities, "total");
      const embedded = sumNumeric(entities, "embedded");
      return [
        { label: "Entity lanes", value: entities.length },
        { label: "Total records", value: total },
        { label: "Embedded", value: embedded },
      ];
    }

    if (typeof payload.retracted === "boolean") {
      return [
        { label: "Retracted", value: payload.retracted ? "yes" : "no" },
        { label: "Entity", value: String(payload.entity_type ?? "unknown") },
        { label: "Decisions closed", value: Number(payload.decisions_closed ?? 0) },
        { label: "Constraints closed", value: Number(payload.constraints_closed ?? 0) },
      ];
    }

    const keys = Object.keys(payload);
    return [
      { label: "Fields", value: keys.length },
      { label: "Shape", value: keys.slice(0, 3).join(", ") || "object" },
    ];
  }

  function resultLists(toolName: string, payload: unknown): SummaryList[] {
    if (!isRecord(payload)) {
      return [{ title: "Value", items: [String(payload)] }];
    }

    if (toolName === "get_graph_snapshot" && Array.isArray(payload.nodes) && Array.isArray(payload.edges)) {
      const nodes = payload.nodes.filter(isRecord);
      const edges = payload.edges.filter(isRecord);
      return [
        {
          title: "Node kinds",
          items: formatCounts(countBy(nodes, (node) => String(node.kind ?? "unknown"))),
        },
        {
          title: "Line types",
          items: formatCounts(countBy(edges, (edge) => String(edge.edge_type ?? "related"))),
        },
      ];
    }

    if (toolName === "index_status" && Array.isArray(payload.entities)) {
      const entities = payload.entities.filter(isRecord);
      return [
        {
          title: "Lanes",
          items: entities.slice(0, 12).map((entity) => {
            const name = String(entity.entity_type ?? entity.kind ?? entity.name ?? "entity");
            const total = String(entity.total ?? "?");
            const embedded = entity.embedded == null ? null : String(entity.embedded);
            return embedded ? `${name}: ${embedded}/${total} embedded` : `${name}: ${total}`;
          }),
        },
      ];
    }

    if (Array.isArray(payload.warnings)) {
      return [{ title: "Warnings", items: payload.warnings.map(String) }];
    }

    return [{ title: "Top-level fields", items: Object.keys(payload).slice(0, 12) }];
  }

  function countBy(items: Record<string, unknown>[], keyFn: (item: Record<string, unknown>) => string): Record<string, number> {
    const counts: Record<string, number> = {};
    for (const item of items) {
      const key = keyFn(item);
      counts[key] = (counts[key] ?? 0) + 1;
    }
    return counts;
  }

  function formatCounts(counts: Record<string, number>): string[] {
    return Object.entries(counts)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 12)
      .map(([key, count]) => `${key}: ${count}`);
  }

  function sumNumeric(items: Record<string, unknown>[], key: string): number {
    return items.reduce((total, item) => total + Number(item[key] ?? 0), 0);
  }

  function initFormValues(tool: ToolDescriptor): Record<string, unknown> {
    const schema = tool.inputSchema;
    const props = isRecord(schema?.properties)
      ? (schema.properties as Record<string, unknown>)
      : {};
    const next: Record<string, unknown> = {};
    for (const [key, prop] of Object.entries(props)) {
      const p = isRecord(prop) ? prop : {};
      if (key === "repo_path") {
        next[key] = repoPath;
        continue;
      }
      if (p.default !== undefined) {
        next[key] = p.default;
        continue;
      }
      const type = schemaType(p);
      if (type === "boolean") next[key] = false;
      else if (type === "integer" || type === "number") next[key] = undefined;
      else next[key] = "";
    }
    return next;
  }

  function buildFormArgs(): Record<string, unknown> {
    const args: Record<string, unknown> = {};
    for (const [key, val] of Object.entries(formValues)) {
      if (key === "repo_path") continue;
      if (val === "" || val === null || val === undefined) continue;
      args[key] = val;
    }
    args.repo_path = repoPath;
    return args;
  }

  function toggleJsonMode() {
    if (jsonMode) {
      try {
        const parsed = parseObj(toolInput);
        const next: Record<string, unknown> = { ...formValues };
        for (const [k, v] of Object.entries(parsed)) {
          if (k !== "repo_path") next[k] = v;
        }
        formValues = next;
      } catch {
        /* keep formValues as-is */
      }
      jsonMode = false;
    } else {
      toolInput = JSON.stringify(buildFormArgs(), null, 2);
      jsonMode = true;
    }
  }

  async function runTool(
    name = selectedTool,
    argsOverride?: Record<string, unknown>,
  ) {
    if (!client) {
      errorMsg = "Not connected.";
      return;
    }
    if (name !== selectedTool) {
      const tool = tools.find((candidate) => candidate.name === name);
      if (tool && name !== "get_graph_snapshot") selectTool(tool);
    }
    resultMode = "summary";
    running = true;
    errorMsg = null;
    status = `Running ${name}…`;

    try {
      let args: Record<string, unknown>;
      if (argsOverride) {
        args = argsOverride;
      } else if (jsonMode) {
        args = parseObj(toolInput);
      } else {
        args = buildFormArgs();
      }
      if (!args.repo_path) args.repo_path = repoPath;

      const raw = await client.callTool(name, args);
      const payload = extractToolPayload(raw);
      const text = JSON.stringify(payload, null, 2);
      toolResults = {
        ...toolResults,
        [name]: {
          toolName: name,
          ranAt: new Date().toLocaleTimeString(),
          status: "success",
          payload,
          text,
        },
      };

      mergeGraph(payload, true);

      status = `Done · ${name}`;
    } catch (err) {
      const message = toStr(err);
      errorMsg = message;
      toolResults = {
        ...toolResults,
        [name]: {
          toolName: name,
          ranAt: new Date().toLocaleTimeString(),
          status: "error",
          payload: message,
          text: message,
        },
      };
      status = `Failed · ${name}`;
    } finally {
      running = false;
    }
  }

  // graphMode tracks whether the current graph is a focused neighbourhood or global
  let graphMode: "global" | "focused" = "global";
  let graphFocusLabel = "";

  function snapshotLimitArgs(): Record<string, unknown> {
    if (snapshotMode !== "sampled") return {};
    const maxNodes = Number(snapshotMaxNodes);
    const maxEdges = Number(snapshotMaxEdges);
    return {
      max_nodes_per_kind:
        Number.isFinite(maxNodes) && maxNodes > 0 ? maxNodes : 80,
      max_edges: Number.isFinite(maxEdges) && maxEdges > 0 ? maxEdges : 800,
    };
  }

  function setEdgeCloseness(edgeType: string, value: number) {
    edgeCloseness = {
      ...edgeCloseness,
      [edgeType]: Number.isFinite(value) ? value : 42,
    };
    applyGraphForces();
  }

  function setNodeKindVisible(kind: string, visible: boolean) {
    visibleNodeKinds = {
      ...visibleNodeKinds,
      [kind]: visible,
    };
  }

  function setEdgeTypeVisible(edgeType: string, visible: boolean) {
    visibleEdgeTypes = {
      ...visibleEdgeTypes,
      [edgeType]: visible,
    };
    applyGraphRendering();
  }

  async function loadGraph(focus?: string) {
    const args: Record<string, unknown> = { repo_path: repoPath };
    if (focus && focus.trim()) {
      args.focus_symbol = focus.trim();
      args.focus_depth = 2;
      graphMode = "focused";
      graphFocusLabel = focus.trim();
    } else {
      graphMode = "global";
      graphFocusLabel = "";
      Object.assign(args, snapshotLimitArgs());
    }
    await runTool("get_graph_snapshot", args);
  }

  async function focusOnFilter() {
    if (nodeFilter.trim()) {
      await loadGraph(nodeFilter.trim());
    } else {
      await loadGraph();
    }
  }

  // --------------------------------------------------------------------------
  // Ingest with status polling
  // --------------------------------------------------------------------------

  // --------------------------------------------------------------------------
  // Sync changes (incremental — only git-diff'd files since last ingest)
  // --------------------------------------------------------------------------

  async function syncChanges() {
    if (!client) return;
    ingestState = "running";
    ingestMsg = "Syncing changed files…";
    ingestJobId = null;
    startGraphRefresh();
    try {
      const payload = extractToolPayload(
        await client.callTool("sync_incremental", { repo_path: repoPath }),
      );
      const text =
        typeof payload === "string" ? payload : JSON.stringify(payload);
      ingestMsg = text.slice(0, 160);
      ingestState = "done";
      stopGraphRefresh();
      await loadGraph();
    } catch (err) {
      ingestMsg = toStr(err);
      ingestState = "error";
      stopGraphRefresh();
    }
  }

  // --------------------------------------------------------------------------
  // Full re-index (background job with cancel support)
  // --------------------------------------------------------------------------

  async function startIngest() {
    if (!client) return;
    stopIngestPoll();
    ingestState = "running";
    ingestMsg = "Starting…";
    ingestJobId = null;
    startGraphRefresh();

    try {
      const payload = extractToolPayload(
        await client.callTool("ingest_symbols", { repo_path: repoPath }),
      );
      const text =
        typeof payload === "string" ? payload : JSON.stringify(payload);

      const m = text.match(/job[_\s-]?id[:\s]+([a-f0-9-]{36})/i);
      if (m) {
        ingestJobId = m[1]!;
        ingestMsg = `job ${ingestJobId.slice(0, 8)}… running`;
        startIngestPoll();
      } else {
        ingestMsg = text.slice(0, 120);
        ingestState = "done";
        stopGraphRefresh();
        await loadGraph();
      }
    } catch (err) {
      ingestMsg = toStr(err);
      ingestState = "error";
      stopGraphRefresh();
    }
  }

  function selectedFilePath(node: GNode | null): string | null {
    if (!node) return null;
    if (node.kind === "file") {
      return node.detail || node.label || node.id.replace(/^file:/, "");
    }
    if (node.kind === "symbol") return symbolFilePath(node);
    return null;
  }

  function selectedSymbolName(node: GNode | null): string | null {
    if (!node || node.kind !== "symbol") return null;
    return node.label || node.id;
  }

  function canRetractNode(node: GNode | null): boolean {
    return node?.kind === "decision" || node?.kind === "lead" || node?.kind === "constraint";
  }

  function retractionEntityType(node: GNode): string {
    return node.kind === "constraint" ? "constraint" : "decision";
  }

  async function runEntityTool(name: string, args: Record<string, unknown>) {
    await runTool(name, { repo_path: repoPath, ...args });
  }

  async function focusSelectedNeighborhood() {
    if (!selectedNode) return;
    const focus = selectedNode.kind === "file"
      ? selectedFilePath(selectedNode)
      : selectedSymbolName(selectedNode) ?? selectedNode.label;
    if (focus) await loadGraph(focus);
  }

  async function runSelectedBrief() {
    if (!selectedNode) return;
    const file = selectedFilePath(selectedNode);
    const symbol = selectedSymbolName(selectedNode);
    if (file) {
      await runEntityTool("focused_file_brief", { file });
    } else if (symbol) {
      await runEntityTool("focused_file_brief", { symbol });
    }
  }

  async function runSelectedGovernanceLookup() {
    if (!selectedNode) return;
    const file = selectedFilePath(selectedNode);
    const symbol = selectedSymbolName(selectedNode);
    if (file) {
      await runEntityTool("find_decisions_for_code", { target: { file } });
    } else if (symbol) {
      await runEntityTool("find_decisions_for_code", { target: { symbol } });
    }
  }

  async function runSelectedHistory() {
    if (!selectedNode) return;
    const symbol = selectedSymbolName(selectedNode);
    const file = selectedFilePath(selectedNode);
    const target = symbol ?? file;
    if (target) await runEntityTool("trace_symbol_history", { symbol: target });
  }

  async function runSelectedImpact() {
    if (!selectedNode || selectedNode.kind !== "decision") return;
    await runEntityTool("impact_of", { adr_id: selectedNode.id });
  }

  async function inspectSelectedImpact() {
    if (selectedNode?.kind === "decision") {
      await runSelectedImpact();
      selectWorkspaceTab("advanced");
      return;
    }
    await focusSelectedNeighborhood();
    selectWorkspaceTab("graph");
  }

  async function retractSelectedEntity() {
    if (!client || !selectedNode) return;
    retractRunning = true;
    retractError = null;
    try {
      await client.callTool("retract", {
        repo_path: repoPath,
        entity_id: selectedNode.id,
        entity_type: retractionEntityType(selectedNode),
        reason: retractReason.trim() || "Retracted via entity browser",
      });
      nodeMap.delete(selectedNode.id);
      graph = {
        nodes: [...nodeMap.values()],
        links: graph.links.filter(
          (link) =>
            linkEndpointId(link.source) !== selectedNode!.id &&
            linkEndpointId(link.target) !== selectedNode!.id,
        ),
      };
      selectedNode = null;
      retractReason = "";
    } catch (err) {
      retractError = String(err);
    } finally {
      retractRunning = false;
    }
  }

  async function cancelIngest() {
    if (!client || !ingestJobId) return;
    try {
      await client.callTool("cancel_ingest", { job_id: ingestJobId });
    } catch {
      // poll will detect the cancelled state
    }
  }

  function startIngestPoll() {
    ingestPollTimer = setInterval(async () => {
      if (!client || !ingestJobId) return;
      try {
        const payload = extractToolPayload(
          await client.callTool("ingest_symbols_status", {
            job_id: ingestJobId,
          }),
        );
        const text =
          typeof payload === "string" ? payload : JSON.stringify(payload);
        ingestMsg = text.slice(0, 160);
        if (text.includes("status=done")) {
          ingestState = "done";
          stopIngestPoll();
          stopGraphRefresh();
          await loadGraph();
        } else if (text.includes("status=cancelled")) {
          ingestState = "cancelled";
          stopIngestPoll();
          stopGraphRefresh();
        }
      } catch {
        // ignore transient poll errors
      }
    }, 2000);
  }

  function stopLeadsPoll() {
    if (leadsPollTimer !== null) {
      clearInterval(leadsPollTimer);
      leadsPollTimer = null;
    }
  }

  function startLeadsPoll() {
    leadsPollTimer = setInterval(async () => {
      if (!client || !leadsJobId) return;
      try {
        const payload = extractToolPayload(
          await client.callTool("synthesize_adr_leads_status", { job_id: leadsJobId }),
        );
        const text = typeof payload === "string" ? payload : JSON.stringify(payload);
        if (text.includes('"synthesized"') || text.includes('"leads"')) {
          try {
            const result = JSON.parse(text);
            const s = result.summary ?? result;
            const n = s.synthesized ?? "?";
            const sk = s.skipped ?? "?";
            leadsMsg = `${n} lead${n === 1 ? "" : "s"} synthesized, ${sk} skipped`;
          } catch {
            leadsMsg = text.slice(0, 160);
          }
          leadsState = "done";
          stopLeadsPoll();
        } else {
          leadsMsg = text.slice(0, 160);
        }
      } catch {
        // transient — keep polling
      }
    }, 3000);
  }

  async function startSynthesizeLeads() {
    if (!client) return;
    stopLeadsPoll();
    leadsState = "running";
    leadsMsg = "Starting…";
    leadsJobId = null;
    try {
      const payload = extractToolPayload(
        await client.callTool("synthesize_adr_leads", { repo_path: repoPath }),
      );
      const text = typeof payload === "string" ? payload : JSON.stringify(payload);
      const m = text.match(/job[_\s-]?id[:\s]+([a-f0-9-]{36})/i);
      if (m) {
        leadsJobId = m[1]!;
        leadsMsg = `job ${leadsJobId.slice(0, 8)}… running`;
        startLeadsPoll();
      } else {
        leadsMsg = text.slice(0, 160);
        leadsState = "done";
      }
    } catch (err) {
      leadsMsg = toStr(err);
      leadsState = "error";
    }
  }

  function stopIngestPoll() {
    if (ingestPollTimer !== null) {
      clearInterval(ingestPollTimer);
      ingestPollTimer = null;
    }
  }

  // --------------------------------------------------------------------------
  // Helpers
  // --------------------------------------------------------------------------

  function startGraphRefresh() {
    if (graphRefreshTimer !== null) return;
    graphRefreshTimer = setInterval(async () => {
      if (!client) return;
      try {
        const args: Record<string, unknown> = { repo_path: repoPath };
        if (graphMode === "focused" && graphFocusLabel) {
          args.focus_symbol = graphFocusLabel;
          args.focus_depth = 2;
        } else {
          Object.assign(args, snapshotLimitArgs());
        }
        const payload = extractToolPayload(
          await client.callTool("get_graph_snapshot", args),
        );
        mergeGraph(payload, false);
      } catch {
        /* ignore transient errors */
      }
    }, 3000);
  }

  function stopGraphRefresh() {
    if (graphRefreshTimer !== null) {
      clearInterval(graphRefreshTimer);
      graphRefreshTimer = null;
    }
  }

  function buildTemplate(tool: ToolDescriptor): string {
    const schema = tool.inputSchema;
    const required: string[] = Array.isArray(schema?.required)
      ? (schema.required as string[])
      : [];
    const props = isRecord(schema?.properties)
      ? (schema.properties as Record<string, unknown>)
      : {};
    const seed: Record<string, unknown> = {};

    for (const key of required) {
      if (key === "repo_path") {
        seed[key] = repoPath;
        continue;
      }
      const prop = props[key];
      if (isRecord(prop) && prop.default !== undefined) {
        seed[key] = prop.default;
        continue;
      }
      seed[key] = "";
    }
    if ("repo_path" in props && !("repo_path" in seed))
      seed.repo_path = repoPath;
    return JSON.stringify(seed, null, 2);
  }

  function parseObj(text: string): Record<string, unknown> {
    const v = JSON.parse(text);
    if (!isRecord(v)) throw new Error("Input must be a JSON object");
    return { ...v };
  }

  function symbolFilePath(node: GNode): string | null {
    if (node.kind !== "symbol" || !node.detail) return null;
    const marker = " · ";
    const idx = node.detail.lastIndexOf(marker);
    if (idx < 0) return null;
    const filePath = node.detail.slice(idx + marker.length).trim();
    return filePath.includes("/") || filePath.includes("\\") ? filePath : null;
  }

  function schemaType(schema: Record<string, unknown>): string {
    const direct = schema.type;
    if (typeof direct === "string") return direct;
    if (Array.isArray(direct)) {
      const types = direct.map(String).filter((t) => t !== "null");
      return types[0] ?? "string";
    }
    for (const key of ["anyOf", "oneOf"]) {
      const variants = schema[key];
      if (!Array.isArray(variants)) continue;
      for (const variant of variants) {
        if (!isRecord(variant)) continue;
        const type = schemaType(variant);
        if (type !== "null") return type;
      }
    }
    return "string";
  }

  /**
   * Merge payload into nodeMap, preserving existing GNode object references
   * so the 3d-force-graph simulation keeps x/y/z positions between updates.
   * replace=true: prune nodes absent from the new snapshot and reset selectedNode.
   * replace=false: only add/update — safe to call from background polls.
   */
  function mergeGraph(payload: unknown, replace: boolean): boolean {
    if (
      !isRecord(payload) ||
      !Array.isArray(payload.nodes) ||
      !Array.isArray(payload.edges)
    )
      return false;

    const incoming: GNode[] = (payload.nodes as unknown[])
      .filter(isRecord)
      .map(
        (n): GNode => ({
          id: String(n.id ?? ""),
          kind: String(n.kind ?? "unknown"),
          label: String(n.label ?? n.id ?? "?"),
          detail: n.detail == null ? null : String(n.detail),
        }),
      )
      .filter((n) => n.id);

    const links: GLink[] = (payload.edges as unknown[])
      .filter(isRecord)
      .map(
        (e): GLink => ({
          id: String(e.id ?? `${e.source}-${e.target}`),
          source: String(e.source ?? ""),
          target: String(e.target ?? ""),
          edge_type: String(e.edge_type ?? "related_to"),
          confidence: Number(e.confidence ?? 1),
          cross_file: Boolean(e.cross_file),
        }),
      )
      .filter((l) => l.source && l.target);

    const incomingIds = new Set(incoming.map((n) => n.id));
    const linkIds = new Set(links.map((l) => l.id));

    for (const symbol of incoming) {
      const filePath = symbolFilePath(symbol);
      if (!filePath) continue;

      const fileId = `file:${filePath}`;
      // Prefer a real file node from the snapshot; create one only when the
      // payload has symbols for that file but no file-scoped node.
      if (!incomingIds.has(fileId)) {
        incoming.push({
          id: fileId,
          kind: "file",
          label: filePath,
          detail: filePath,
        });
        incomingIds.add(fileId);
      }

      const linkId = `${PROJECTED_SYMBOL_FILE_EDGE_PREFIX}:${fileId}:${symbol.id}`;
      if (!linkIds.has(linkId)) {
        links.push({
          id: linkId,
          source: fileId,
          target: symbol.id,
          edge_type: "contains",
          confidence: 1,
          cross_file: false,
        });
        linkIds.add(linkId);
      }
    }

    if (replace) {
      const newIds = new Set(incoming.map((n) => n.id));
      for (const id of nodeMap.keys()) {
        if (!newIds.has(id)) nodeMap.delete(id);
      }
      selectedNode = null;
    }

    for (const n of incoming) {
      const ex = nodeMap.get(n.id);
      if (ex) {
        ex.kind = n.kind;
        ex.label = n.label;
        ex.detail = n.detail ?? null;
      } else {
        nodeMap.set(n.id, n);
      }
    }

    graph = { nodes: [...nodeMap.values()], links };
    return true;
  }

  function toStr(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
  }
</script>

<!-- Topbar -->
<nav class="workspace-nav">
  <button class="btn-ghost back-btn" on:click={() => onNavigate("/")}
    >← Back</button
  >
  <div class="workspace-title">
    <span class="eyebrow">Repository</span>
    <h1 title={repoPath}>
      {repoPath.replace(/\\/g, "/").split("/").slice(-2).join("/")}
    </h1>
  </div>
  <div class="status-cluster">
    <span class="status-pill">{status}</span>
    {#if errorMsg}<span class="error-pill">{errorMsg}</span>{/if}
  </div>
</nav>

<section class="workspace-actions" aria-label="Workspace actions">
  <button
    class="qa-btn qa-btn-primary"
    on:click={() => {
      nodeFilter = "";
      loadGraph();
    }}
    disabled={running}>Refresh graph</button
  >
  <details class="action-menu">
    <summary>Sync</summary>
    <div class="action-menu-list">
      <button
        on:click={syncChanges}
        disabled={ingestState === "running"}
        title="Sync only files changed since last ingest (fast)"
      >
        {ingestState === "running" && ingestJobId === null
          ? "Syncing..."
          : "Sync changed files"}
      </button>
      <button
        class="danger-action"
        on:click={startIngest}
        disabled={ingestState === "running"}
        title="Full background re-index with cancel support"
      >
        {ingestState === "running" && ingestJobId !== null
          ? "Indexing..."
          : "Full re-index"}
      </button>
      {#if ingestState === "running" && ingestJobId !== null}
        <button class="danger-action" on:click={cancelIngest}>Cancel index</button>
      {/if}
      <button
        on:click={() =>
          runTool("sync_commits_from_git", { repo_path: repoPath })}
        disabled={running}>Sync commits</button
      >
      <button
        on:click={() => runTool("sync_adrs_from_git", { repo_path: repoPath })}
        disabled={running}>Sync ADRs</button
      >
    </div>
  </details>
  <details class="action-menu">
    <summary>Architecture</summary>
    <div class="action-menu-list">
      <button
        on:click={() => runTool("get_architecture", { repo_path: repoPath })}
        disabled={running}>Architecture summary</button
      >
      <button
        on:click={() => runTool("index_status", { repo_path: repoPath })}
        disabled={running}>Index status</button
      >
    </div>
  </details>
  <details class="action-menu">
    <summary>Generate</summary>
    <div class="action-menu-list">
      <button
        on:click={startSynthesizeLeads}
        disabled={leadsState === "running"}
        title="Synthesize ADR leads from undocumented code patterns"
      >
        {leadsState === "running" ? "Synthesizing..." : "Synthesize leads"}
      </button>
    </div>
  </details>
</section>

<div class="workspace-grid">
  <!-- Left: Tool panel -->
  <aside class="panel tool-panel">
    <div class="navigator-tabs" aria-label="Navigator sections">
      <button
        class:active={navigatorTab === "entities"}
        on:click={() => selectNavigatorTab("entities")}
      >
        Entities <span>{entityRows.length}</span>
      </button>
      <button
        class:active={navigatorTab === "tools"}
        on:click={() => selectNavigatorTab("tools")}
      >
        Tools <span>{filteredTools.length}</span>
      </button>
    </div>

    {#if navigatorTab === "entities"}
      <div class="entity-browser-controls">
        <input
          class="entity-search"
          placeholder="Search entities..."
          bind:value={entitySearch}
        />
        <select class="entity-kind-select" bind:value={entityKindFilter}>
          <option value="all">All kinds</option>
          {#each entityKindOptions as kind}
            <option value={kind}>{kind}</option>
          {/each}
        </select>
      </div>

      <div class="entity-filter-row">
        <select class="entity-sort-select" bind:value={entityGroupFilter}>
          {#each entityGroupSummaries as group (group.key)}
            <option value={group.key}>{group.label} ({group.matching})</option>
          {/each}
        </select>
        <select class="entity-sort-select" bind:value={entitySort}>
          <option value="relevance">Relevance</option>
          <option value="relationships">Relationship count</option>
          <option value="name">Name</option>
          <option value="kind">Kind</option>
          <option value="recent">Recent</option>
        </select>
      </div>

      <div class="entity-kind-grid">
        <button
          class:active={entityKindFilter === "all"}
          on:click={() => (entityKindFilter = "all")}
        >
          <span>All</span>
          <strong>{entityMatches.length}</strong>
        </button>
        {#each entityKindSummaries as summary (summary.kind)}
          <button
            class:active={entityKindFilter === summary.kind}
            on:click={() => (entityKindFilter = summary.kind)}
          >
            <span>
              <span
                class="entity-kind-dot"
                style="background:{KIND_COLORS[summary.kind] ?? '#aaaaaa'}"
              ></span>
              {summary.kind}
            </span>
            <strong>{summary.matching}</strong>
          </button>
        {/each}
      </div>

      <div class="entity-list-head">
        <span>{entityKindFilter === "all" ? "All entities" : entityKindFilter}</span>
        <span>{entityRows.length} shown</span>
      </div>

      <div class="entity-list">
        {#each entityRows as node (node.id)}
          <button
            class="entity-row"
            class:active={selectedNode?.id === node.id}
            title={entityTitle(node)}
            on:click={() => selectNode(node)}
          >
            <span
              class="entity-kind-dot"
              style="background:{KIND_COLORS[node.kind] ?? '#aaaaaa'}"
            ></span>
            <span class="entity-row-main">
              <strong>{node.label}</strong>
              <span>{entityDetail(node)}</span>
              {#if entityPath(node)}
                <em>{entityPath(node)}</em>
              {/if}
            </span>
            <span class="entity-rel-count">
              {relationshipCounts[node.id] ?? 0}
            </span>
          </button>
        {/each}
        {#if entityRows.length === 0}
          <p class="entity-empty">No entities</p>
        {/if}
      </div>
    {:else}
      <input class="tool-search" placeholder="Filter tools..." bind:value={toolSearch} />

      <div class="tool-category-grid">
        <button
          class:active={toolCategoryFilter === "all"}
          on:click={() => (toolCategoryFilter = "all")}
        >
          <span>All</span>
          <strong>{toolMatches.length}</strong>
        </button>
        {#each toolCategorySummaries as summary (summary.category)}
          <button
            class:active={toolCategoryFilter === summary.category}
            on:click={() => (toolCategoryFilter = summary.category)}
          >
            <span>{summary.category}</span>
            <strong>{summary.count}</strong>
          </button>
        {/each}
      </div>

      <div class="tool-list-head">
        <span>{toolCategoryFilter === "all" ? "All tools" : toolCategoryFilter}</span>
        <span>{filteredTools.length} shown</span>
      </div>

      <div class="tool-list">
        {#each filteredTools as tool}
          <button
            class="tool-item"
            class:active={tool.name === selectedTool}
            title={tool.description ?? tool.name}
            on:click={() => selectTool(tool)}
          >
            <span class="tool-item-head">
              <strong>{tool.name}</strong>
              <em>{toolCategory(tool)}</em>
            </span>
            <span class="tool-item-desc">{tool.description ?? ""}</span>
          </button>
        {/each}
        {#if filteredTools.length === 0}
          <p class="entity-empty">No tools</p>
        {/if}
      </div>
    {/if}

    <!-- Leads synthesis progress indicator -->
    {#if leadsState !== "idle"}
      <div
        class="ingest-status"
        class:done={leadsState === "done"}
        class:error={leadsState === "error"}
      >
        <div class="ingest-status-head">
          {#if leadsState === "running"}
            <span class="spinner" aria-hidden="true"></span>
          {:else if leadsState === "done"}
            <span class="ingest-icon ok">✓</span>
          {:else}
            <span class="ingest-icon err">✗</span>
          {/if}
          <span class="ingest-label">
            {leadsState === "running" ? "Synthesizing leads" : leadsState === "done" ? "Leads done" : "Error"}
          </span>
          {#if leadsState !== "running"}
            <button
              class="ingest-dismiss"
              on:click={() => { leadsState = "idle"; leadsMsg = ""; }}>×</button
            >
          {/if}
        </div>
        {#if leadsMsg}
          <p class="ingest-msg">{leadsMsg}</p>
        {/if}
      </div>
    {/if}

    <!-- Ingest progress indicator -->
    {#if ingestState !== "idle"}
      <div
        class="ingest-status"
        class:done={ingestState === "done"}
        class:error={ingestState === "error"}
        class:cancelled={ingestState === "cancelled"}
      >
        <div class="ingest-status-head">
          {#if ingestState === "running"}
            <span class="spinner" aria-hidden="true"></span>
          {:else if ingestState === "done"}
            <span class="ingest-icon ok">✓</span>
          {:else if ingestState === "cancelled"}
            <span class="ingest-icon cancelled">⊘</span>
          {:else}
            <span class="ingest-icon err">✗</span>
          {/if}
          <span class="ingest-label">
            {ingestState === "running"
              ? "Indexing"
              : ingestState === "done"
                ? "Done"
                : ingestState === "cancelled"
                  ? "Cancelled"
                  : "Error"}
          </span>
          {#if ingestState !== "running"}
            <button
              class="ingest-dismiss"
              on:click={() => {
                ingestState = "idle";
                ingestMsg = "";
              }}>×</button
            >
          {/if}
        </div>
        {#if ingestMsg}
          <p class="ingest-msg">{ingestMsg}</p>
        {/if}
      </div>
    {/if}
  </aside>

  <!-- Middle: workspace -->
  <section class="panel workspace-panel">
    <div class="workspace-nav-groups" aria-label="Workspace sections">
      <div class="workspace-tab-group">
        <span>Graph</span>
        <div class="workspace-tabs">
          {#each WORKSPACE_VIEW_TABS as tab}
            <button
              class:active={workspaceTab === tab.key}
              on:click={() => selectWorkspaceTab(tab.key)}
            >
              {tab.label}
            </button>
          {/each}
        </div>
      </div>
      <div class="workspace-tab-group">
        <span>Entity</span>
        <div class="workspace-tabs entity-tabs">
          {#each ENTITY_WORKSPACE_TABS as tab}
            <button
              class:active={workspaceTab === tab.key}
              disabled={!selectedNode}
              on:click={() => selectWorkspaceTab(tab.key)}
            >
              {tab.label}
            </button>
          {/each}
        </div>
      </div>
    </div>

    {#if workspaceTab !== "graph" && workspaceTab !== "advanced"}
      {#if selectedNode}
        <div class="entity-workspace-head">
          <div class="entity-heading">
            <span
              class="node-kind"
              style="color:{KIND_COLORS[selectedNode.kind] ?? 'var(--cyan)'}"
            >
              {selectedNode.kind === "lead" ? "observed pattern" : selectedNode.kind}
            </span>
            <h2>{selectedNode.label}</h2>
            <p>{entityDetail(selectedNode)}</p>
          </div>
          <div class="entity-primary-actions">
            <button on:click={inspectSelectedImpact} disabled={running}>Inspect impact</button>
            <button on:click={runSelectedGovernanceLookup} disabled={running}>Find docs</button>
          </div>
        </div>

        <dl class="workspace-facts">
          <div>
            <dt>Kind</dt>
            <dd>{selectedNode.kind}</dd>
          </div>
          {#if entityPath(selectedNode)}
            <div>
              <dt>Path</dt>
              <dd>{entityPath(selectedNode)}</dd>
            </div>
          {/if}
          <div>
            <dt>Relationships</dt>
            <dd>{selectedRelationRows.length}</dd>
          </div>
          <div>
            <dt>ID</dt>
            <dd>{selectedNode.id}</dd>
          </div>
        </dl>
      {:else}
        <div class="workspace-empty">
          <h2>Select an entity</h2>
          <p>Choose a graph node or navigator row before inspecting entity details.</p>
        </div>
      {/if}
    {/if}

    <div class="workspace-tab-body">
      {#if !selectedNode && workspaceTab !== "advanced" && workspaceTab !== "graph"}
        <div class="soft-empty">Choose an entity from the navigator to start an investigation.</div>
      {:else if workspaceTab === "overview"}
        <div class="overview-grid">
          <section class="summary-card">
            <span>Relationships</span>
            <strong>{selectedRelationRows.length}</strong>
            <p>{formatCounts(selectedRelationTypeCounts).slice(0, 4).join(" · ") || "No loaded relationships"}</p>
          </section>
          <section class="summary-card">
            <span>Docs & claims</span>
            <strong>{selectedDocsAndClaims.length}</strong>
            <p>Only loaded graph evidence is shown.</p>
          </section>
          <section class="summary-card">
            <span>History</span>
            <strong>{selectedHistoryRows.length}</strong>
            <p>Commit and change edges currently loaded.</p>
          </section>
        </div>

        <div class="relation-panel workspace-relation-panel">
          <div class="relation-head">
            <h3>Top relationships</h3>
            <button on:click={() => selectWorkspaceTab("relationships")}>View table</button>
          </div>
          <div class="relation-list">
            {#each selectedRelationRows.slice(0, 8) as row (row.id)}
              <button
                class="relation-row"
                on:click={() => {
                  selectedRelationshipId = row.id;
                  selectWorkspaceTab("relationships");
                }}
              >
                <span class="relation-type">{row.direction === "out" ? "to" : "from"} · {row.relation}</span>
                <span class="relation-label">{row.other?.label ?? row.otherId}</span>
                <span class="relation-meta">{row.edge.confidence.toFixed(2)}</span>
              </button>
            {/each}
            {#if selectedRelationRows.length === 0}
              <p class="relation-empty">No relationships loaded.</p>
            {/if}
          </div>
        </div>

        {#if canRetractNode(selectedNode)}
          <div class="node-actions">
            <input
              class="retract-reason"
              type="text"
              placeholder="Reason (optional)"
              bind:value={retractReason}
              disabled={retractRunning}
            />
            <button
              class="btn-retract"
              disabled={retractRunning}
              on:click={retractSelectedEntity}
            >
              {retractRunning ? "Retracting..." : "Retract"}
            </button>
            {#if retractError}
              <p class="retract-error">{retractError}</p>
            {/if}
          </div>
        {/if}
      {:else if workspaceTab === "relationships"}
        <div class="relationship-workspace">
          <div class="relationship-table-wrap">
            <table class="relationship-table">
              <thead>
                <tr>
                  <th>Related entity</th>
                  <th>Relation</th>
                  <th>Source</th>
                  <th>Confidence</th>
                </tr>
              </thead>
              <tbody>
                {#each selectedRelationRows as row (row.id)}
                  <tr class:active={selectedRelationship?.id === row.id}>
                    <td>
                      <button
                        class="table-entity-button"
                        on:click={() => {
                          selectedRelationshipId = row.id;
                          if (row.other) selectNode(row.other, false);
                        }}
                      >
                        <strong>{row.other?.label ?? row.otherId}</strong>
                        <span>{row.other ? entityDetail(row.other) : row.otherId}</span>
                      </button>
                    </td>
                    <td>{row.direction === "out" ? "to" : "from"} · {row.relation}</td>
                    <td>{row.sourceLabel}</td>
                    <td>{row.edge.confidence.toFixed(2)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
            {#if selectedRelationRows.length === 0}
              <div class="soft-empty">No relationships loaded for this entity.</div>
            {/if}
          </div>
          <aside class="why-panel">
            <h3>Why related?</h3>
            {#if selectedRelationship}
              <p><strong>{selectedNode?.label}</strong> {selectedRelationship.direction === "out" ? "connects to" : "is connected from"} <strong>{selectedRelationship.other?.label ?? selectedRelationship.otherId}</strong>.</p>
              <dl class="entity-facts">
                <div>
                  <dt>Edge</dt>
                  <dd>{selectedRelationship.relation}</dd>
                </div>
                <div>
                  <dt>Evidence</dt>
                  <dd>{selectedRelationship.sourceLabel}</dd>
                </div>
                <div>
                  <dt>Trust</dt>
                  <dd>{isProjectedFileLink(selectedRelationship.edge) ? "UI-only projection from symbol metadata" : "Loaded graph relationship"}</dd>
                </div>
              </dl>
            {:else}
              <p>Select a relationship row to inspect available evidence.</p>
            {/if}
          </aside>
        </div>
      {:else if workspaceTab === "history"}
        <div class="lane-list">
          {#each selectedHistoryRows as row (row.id)}
            <button class="lane-row" on:click={() => row.other && selectNode(row.other)}>
              <strong>{row.other?.label ?? row.otherId}</strong>
              <span>{row.relation} · {row.sourceLabel}</span>
            </button>
          {/each}
          {#if selectedHistoryRows.length === 0}
            <div class="soft-empty">No loaded commit or change relationships for this entity.</div>
          {/if}
        </div>
      {:else if workspaceTab === "docs"}
        <div class="lane-list">
          {#each selectedDocsAndClaims as row (row.id)}
            <button class="lane-row" on:click={() => row.other && selectNode(row.other)}>
              <strong>{row.other?.label ?? row.otherId}</strong>
              <span>{row.other?.kind ?? "relationship"} · {row.relation} · {row.sourceLabel}</span>
            </button>
          {/each}
          {#if selectedDocsAndClaims.length === 0}
            <div class="soft-empty">No loaded docs, decisions, leads, or claim-like relationships. Run a docs/governance tool from MCP calls to add evidence.</div>
          {/if}
        </div>
      {/if}

      <div class="workspace-tab-panel graph-workspace" class:inactive={workspaceTab !== "graph"}>
        <div class="panel-head">
          <h2>
            Relationship graph
            {#if graphMode === "focused"}
              <span class="graph-mode-badge focused">⌖ {graphFocusLabel}</span>
            {:else}
              <span class="graph-mode-badge global">global</span>
            {/if}
          </h2>
          <span class="count">{displayGraph.nodes.length}N / {visibleEdgeCount}L</span>
        </div>
        <div class="graph-filter-row">
          <input
            class="graph-filter"
            placeholder="Filter by name... (Enter = focused load)"
            bind:value={nodeFilter}
            title="Type to filter client-side. Press Enter to reload server-side focused on this symbol."
            on:keydown={(e) => {
              if (e.key === "Enter") focusOnFilter();
            }}
          />
          {#if nodeFilter}
            <button
              class="graph-filter-clear"
              title="Clear filter"
              on:click={() => {
                nodeFilter = "";
                if (graphMode === "focused") loadGraph();
              }}>×</button
            >
          {/if}
          <button
            class="graph-filter-load"
            title="Load focused neighbourhood from server"
            on:click={focusOnFilter}
            disabled={running}>⌖</button
          >
        </div>
        <div class="snapshot-controls">
          <div class="snapshot-mode" aria-label="Snapshot mode">
            <button
              class:active={snapshotMode === "full"}
              title="Load the full global snapshot"
              on:click={() => {
                snapshotMode = "full";
                if (graphMode === "global") loadGraph();
              }}>Full</button
            >
            <button
              class:active={snapshotMode === "sampled"}
              title="Load a capped global snapshot"
              on:click={() => {
                snapshotMode = "sampled";
                if (graphMode === "global") loadGraph();
              }}>Sampled</button
            >
          </div>
          {#if snapshotMode === "sampled"}
            <label class="snapshot-limit">
              <span>nodes</span>
              <input
                type="number"
                min="1"
                bind:value={snapshotMaxNodes}
                on:change={() => {
                  if (graphMode === "global") loadGraph();
                }}
              />
            </label>
            <label class="snapshot-limit">
              <span>edges</span>
              <input
                type="number"
                min="1"
                bind:value={snapshotMaxEdges}
                on:change={() => {
                  if (graphMode === "global") loadGraph();
                }}
              />
            </label>
          {/if}
          <button
            class="snapshot-refresh"
            title="Reload graph"
            disabled={running}
            on:click={() =>
              loadGraph(graphMode === "focused" ? graphFocusLabel : undefined)}
            >↻</button
          >
          <button
            class="layout-toggle"
            class:active={showLayoutControls}
            title="Layout controls"
            on:click={() => (showLayoutControls = !showLayoutControls)}
            >Layout</button
          >
        </div>
        {#if showLayoutControls}
          <div class="layout-controls">
            <label class="layout-slider layout-slider-wide">
              <span>file clustering</span>
              <input
                type="range"
                min="0"
                max="100"
                bind:value={fileCloseness}
                on:input={applyGraphForces}
              />
              <output>{fileCloseness}</output>
            </label>
            <label class="layout-slider layout-slider-wide">
              <span>file spacing</span>
              <input
                type="range"
                min="0"
                max="100"
                bind:value={fileSpacing}
                on:input={applyGraphForces}
              />
              <output>{fileSpacing}</output>
            </label>
            <div class="edge-layout-grid">
              {#each Object.keys(EDGE_COLORS) as type}
                <label class="layout-slider">
                  <span>{type.replace(/_/g, " ")}</span>
                  <input
                    type="range"
                    min="0"
                    max="100"
                    value={edgeCloseness[type] ?? 42}
                    on:input={(e) =>
                      setEdgeCloseness(type, e.currentTarget.valueAsNumber)}
                  />
                  <output>{edgeCloseness[type] ?? 42}</output>
                </label>
              {/each}
            </div>
          </div>
        {/if}
        <div class="graph-surface" bind:this={graphEl}></div>
        <div class="graph-legend">
          <span class="legend-section">Nodes</span>
          {#each Object.entries(KIND_COLORS) as [kind, color]}
            <label class="legend-item legend-toggle" class:muted={!visibleNodeKinds[kind]}>
              <input
                type="checkbox"
                checked={visibleNodeKinds[kind] ?? true}
                on:change={(e) => setNodeKindVisible(kind, e.currentTarget.checked)}
              />
              <span class="legend-dot" style="background:{color}"></span>
              {kind}
              <span class="legend-count">{nodeCounts[kind] ?? 0}</span>
            </label>
          {/each}
          <span class="legend-sep"></span>
          <span class="legend-section">Lines</span>
          {#each Object.entries(EDGE_COLORS) as [type, color]}
            <label class="legend-item legend-toggle" class:muted={!visibleEdgeTypes[type]}>
              <input
                type="checkbox"
                checked={visibleEdgeTypes[type] ?? false}
                on:change={(e) => setEdgeTypeVisible(type, e.currentTarget.checked)}
              />
              <span
                class="legend-line"
                style="background:{color};box-shadow:0 0 3px {color}"
              ></span>
              {type.replace(/_/g, " ")}
              <span class="legend-count">{edgeCounts[type] ?? 0}</span>
            </label>
          {/each}
          <span class="legend-item legend-crossfile">
            <span class="legend-line crossfile"></span>
            cross-file calls
            <span class="legend-count">{edgeCounts.cross_file_calls ?? 0}</span>
          </span>
        </div>
      </div>

      {#if workspaceTab === "advanced"}
        <div class="advanced-workspace">
          <div class="panel-head">
            <h2>MCP call · <span class="tool-name">{selectedTool || "select a tool"}</span></h2>
            <div class="cmd-actions">
              {#if selectedTool}
                <button
                  class="btn-mode"
                  on:click={toggleJsonMode}
                  title={jsonMode ? "Switch to guided form" : "Switch to raw JSON"}
                >
                  {jsonMode ? "⊞ Form" : "{ } JSON"}
                </button>
              {/if}
              <button
                class="btn-run"
                on:click={() => runTool()}
                disabled={running || !selectedTool}
              >
                {running ? "Running..." : "Run ▶"}
              </button>
            </div>
          </div>

          {#if currentTool?.description}
            <p class="tool-desc">{currentTool.description}</p>
          {/if}

          {#if jsonMode || !selectedTool}
            <textarea class="json-editor" spellcheck="false" bind:value={toolInput}></textarea>
          {:else}
            <div class="tool-form">
              {#each formFields as field (field.key)}
                {#if field.key === "repo_path"}
                  <div class="form-repo-row">
                    <span class="form-repo-tag">repo</span>
                    <span class="form-repo-path">{repoPath}</span>
                  </div>
                {:else}
                  <div class="form-field">
                    <div class="form-field-head">
                      <label class="form-label" for="ff-{field.key}">{field.key}</label>
                      {#if field.required}<span class="form-req">required</span>{/if}
                    </div>
                    {#if field.description}
                      <p class="form-hint">{field.description}</p>
                    {/if}
                    {#if field.enum}
                      <select
                        id="ff-{field.key}"
                        class="form-select"
                        on:change={(e) => {
                          formValues = {
                            ...formValues,
                            [field.key]: e.currentTarget.value,
                          };
                        }}
                      >
                        {#if !field.required}<option value="" selected={!formValues[field.key]}>— any —</option>{/if}
                        {#each field.enum as opt}
                          <option value={opt} selected={formValues[field.key] === opt}>{opt}</option>
                        {/each}
                      </select>
                    {:else if field.type === "boolean"}
                      <label class="form-toggle">
                        <input
                          type="checkbox"
                          checked={!!formValues[field.key]}
                          on:change={(e) => {
                            formValues = {
                              ...formValues,
                              [field.key]: e.currentTarget.checked,
                            };
                          }}
                        />
                        <span class="toggle-track"><span class="toggle-thumb"></span></span>
                        <span class="toggle-label">{formValues[field.key] ? "enabled" : "disabled"}</span>
                      </label>
                    {:else if field.type === "integer" || field.type === "number"}
                      <input
                        id="ff-{field.key}"
                        type="number"
                        class="form-input"
                        value={formValues[field.key] ?? ""}
                        on:input={(e) => {
                          formValues = {
                            ...formValues,
                            [field.key]:
                              e.currentTarget.value === ""
                                ? undefined
                                : Number(e.currentTarget.value),
                          };
                        }}
                      />
                    {:else if field.multiline}
                      <textarea
                        id="ff-{field.key}"
                        class="form-textarea"
                        rows="3"
                        placeholder={field.required ? "required" : "optional"}
                        value={String(formValues[field.key] ?? "")}
                        on:input={(e) => {
                          formValues = {
                            ...formValues,
                            [field.key]: e.currentTarget.value,
                          };
                        }}
                      ></textarea>
                    {:else}
                      <input
                        id="ff-{field.key}"
                        type="text"
                        class="form-input"
                        placeholder={field.required ? "required" : "optional"}
                        value={String(formValues[field.key] ?? "")}
                        on:input={(e) => {
                          formValues = {
                            ...formValues,
                            [field.key]: e.currentTarget.value,
                          };
                        }}
                      />
                    {/if}
                  </div>
                {/if}
              {/each}
              {#if formFields.length === 0 && selectedTool}
                <p class="form-empty">No parameters — click Run ▶ to execute.</p>
              {/if}
            </div>
          {/if}

          <div class="panel-head section-gap result-head">
            <h2>
              Result
              {#if currentToolResult}
                <span class="result-tool-name">{currentToolResult.toolName}</span>
              {/if}
            </h2>
            {#if currentToolResult}
              <div class="result-actions">
                <span class:success={currentToolResult.status === "success"} class:error={currentToolResult.status === "error"}>
                  {currentToolResult.status} · {currentToolResult.ranAt}
                </span>
                <button class:active={resultMode === "summary"} on:click={() => (resultMode = "summary")}>Summary</button>
                <button class:active={resultMode === "json"} on:click={() => (resultMode = "json")}>JSON</button>
              </div>
            {/if}
          </div>
          {#if !selectedTool}
            <div class="result-empty">Select a tool to run commands.</div>
          {:else if !currentToolResult}
            <div class="result-empty">No result for {selectedTool} yet.</div>
          {:else if resultMode === "json"}
            <pre class="result-pane">{currentToolResult.text}</pre>
          {:else}
            <div class="result-summary">
              <div class="result-metrics">
                {#each currentResultMetrics as metric}
                  <div class="result-metric">
                    <span>{metric.label}</span>
                    <strong>{metric.value}</strong>
                  </div>
                {/each}
              </div>
              <div class="result-summary-lists">
                {#each currentResultLists as list}
                  <section>
                    <h3>{list.title}</h3>
                    <ul>
                      {#each list.items as item}
                        <li>{item}</li>
                      {/each}
                    </ul>
                  </section>
                {/each}
              </div>
            </div>
          {/if}
        </div>
      {/if}
    </div>
  </section>

</div>

<style>
  .workspace-nav {
    display: flex;
    align-items: center;
    gap: 1rem;
    min-height: 64px;
    padding: 0.75rem 1.5rem;
    border-bottom: 1px solid var(--panel-border);
    flex-wrap: wrap;
  }

  :global(body:has(.workspace-grid)) {
    overflow: hidden;
  }

  .workspace-title h1 {
    margin: 0;
    font-size: 1.1rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 340px;
  }

  .workspace-title .eyebrow {
    margin: 0;
  }

  .status-cluster {
    margin-left: auto;
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    align-items: center;
  }

  .status-pill {
    padding: 0.35rem 0.75rem;
    border-radius: 999px;
    font-size: 0.82rem;
    background: rgba(110, 231, 255, 0.12);
    color: var(--cyan);
  }

  .error-pill {
    padding: 0.35rem 0.75rem;
    border-radius: 999px;
    font-size: 0.82rem;
    background: rgba(244, 132, 95, 0.14);
    color: var(--coral);
    max-width: 320px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .btn-ghost {
    background: transparent;
    color: var(--muted);
    border: 1px solid var(--panel-border);
    padding: 0.45rem 0.9rem;
    border-radius: 12px;
    font-size: 0.9rem;
    box-shadow: none;
  }

  .btn-ghost:hover {
    color: var(--text);
    border-color: rgba(110, 231, 255, 0.35);
  }

  .workspace-actions {
    position: relative;
    z-index: 40;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.45rem;
    min-height: 48px;
    padding: 0.55rem 1.5rem;
    border-bottom: 1px solid rgba(102, 213, 255, 0.1);
    overflow: visible;
  }

  .action-menu {
    position: relative;
  }

  .action-menu[open] {
    z-index: 50;
  }

  .action-menu summary {
    list-style: none;
    cursor: pointer;
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    padding: 0.42rem 0.65rem;
    font-size: 0.78rem;
    user-select: none;
  }

  .action-menu summary::-webkit-details-marker {
    display: none;
  }

  .action-menu summary::after {
    content: " ▾";
    color: var(--muted);
  }

  .action-menu-list {
    position: absolute;
    top: calc(100% + 0.35rem);
    left: 0;
    z-index: 60;
    min-width: 190px;
    display: grid;
    gap: 0.25rem;
    padding: 0.4rem;
    border: 1px solid rgba(110, 231, 255, 0.18);
    border-radius: 10px;
    background: rgba(4, 15, 24, 0.96);
    box-shadow: var(--shadow);
  }

  .action-menu-list button {
    text-align: left;
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 7px;
    background: rgba(255, 255, 255, 0.035);
    color: var(--text);
    box-shadow: none;
    padding: 0.42rem 0.55rem;
    font-size: 0.78rem;
  }

  .action-menu-list button:hover:not(:disabled) {
    border-color: rgba(110, 231, 255, 0.3);
  }

  .action-menu-list .danger-action {
    color: #ffd0c2;
    border-color: rgba(244, 132, 95, 0.34);
    background: rgba(244, 132, 95, 0.08);
  }

  .workspace-grid {
    position: relative;
    z-index: 1;
    display: grid;
    grid-template-columns: 320px minmax(560px, 1fr);
    gap: 1rem;
    padding: 1rem 1.5rem;
    align-items: stretch;
    height: calc(100vh - 112px);
    min-height: 0;
    overflow: hidden;
  }

  .panel {
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 14px;
    padding: 1rem;
    backdrop-filter: blur(20px);
    box-shadow: var(--shadow);
    min-width: 0;
    min-height: 0;
  }

  .tool-panel {
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .workspace-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }

  .panel-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 0.75rem;
  }

  .panel-head h2 {
    margin: 0;
    font-size: 0.85rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--muted);
  }

  .count {
    font-size: 0.82rem;
    color: var(--muted);
  }

  .section-gap {
    margin-top: 0.75rem;
  }

  .navigator-tabs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.35rem;
    margin-bottom: 0.7rem;
  }

  .navigator-tabs button {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.45rem;
    background: rgba(4, 15, 24, 0.7);
    color: var(--muted);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    box-shadow: none;
    padding: 0.45rem 0.55rem;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .navigator-tabs button.active {
    color: var(--cyan);
    border-color: rgba(110, 231, 255, 0.35);
    background: rgba(110, 231, 255, 0.1);
  }

  .navigator-tabs span {
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.72rem;
    letter-spacing: 0;
  }

  .entity-browser-controls {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 104px;
    gap: 0.4rem;
    margin-bottom: 0.55rem;
    flex-shrink: 0;
  }

  .entity-filter-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 0.4rem;
    margin-bottom: 0.55rem;
    flex-shrink: 0;
  }

  .entity-sort-select {
    min-width: 0;
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(110, 231, 255, 0.15);
    border-radius: 8px;
    padding: 0.48rem 0.45rem;
    font-size: 0.76rem;
  }

  .entity-search,
  .entity-kind-select,
  .tool-search {
    margin-bottom: 0.6rem;
    padding: 0.55rem 0.85rem;
    font-size: 0.88rem;
  }

  .entity-search,
  .entity-kind-select {
    margin-bottom: 0;
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(110, 231, 255, 0.15);
    border-radius: 8px;
    min-width: 0;
  }

  .entity-kind-select {
    padding: 0.55rem 0.45rem;
  }

  .entity-kind-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 0.35rem;
    margin-bottom: 0.55rem;
    flex-shrink: 0;
  }

  .entity-kind-grid button {
    min-width: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.45rem;
    background: rgba(4, 15, 24, 0.68);
    color: var(--muted);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    box-shadow: none;
    padding: 0.42rem 0.5rem;
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.07em;
  }

  .entity-kind-grid button.active {
    color: var(--cyan);
    border-color: rgba(110, 231, 255, 0.42);
    background: rgba(110, 231, 255, 0.11);
  }

  .entity-kind-grid span {
    min-width: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .entity-kind-grid strong {
    flex-shrink: 0;
    color: rgba(255, 255, 255, 0.58);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.68rem;
    letter-spacing: 0;
  }

  .tool-category-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 0.35rem;
    margin-bottom: 0.55rem;
    flex-shrink: 0;
  }

  .tool-category-grid button {
    min-width: 0;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.4rem;
    background: rgba(4, 15, 24, 0.68);
    color: var(--muted);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    box-shadow: none;
    padding: 0.42rem 0.5rem;
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.07em;
  }

  .tool-category-grid button.active {
    color: var(--cyan);
    border-color: rgba(110, 231, 255, 0.42);
    background: rgba(110, 231, 255, 0.11);
  }

  .tool-category-grid span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .tool-category-grid strong {
    flex-shrink: 0;
    color: rgba(255, 255, 255, 0.58);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.68rem;
    letter-spacing: 0;
  }

  .tool-list-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.35rem;
    color: rgba(255, 255, 255, 0.42);
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    flex-shrink: 0;
  }

  .tool-list-head span:last-child {
    font-family: "Cascadia Code", "Fira Code", monospace;
    letter-spacing: 0;
    text-transform: none;
  }

  .entity-list-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.35rem;
    color: rgba(255, 255, 255, 0.42);
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    flex-shrink: 0;
  }

  .entity-list-head span:last-child {
    font-family: "Cascadia Code", "Fira Code", monospace;
    letter-spacing: 0;
    text-transform: none;
  }

  .entity-list {
    display: grid;
    gap: 0.28rem;
    align-content: start;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overflow-x: hidden;
    padding-right: 0.15rem;
  }

  .entity-row {
    width: 100%;
    display: grid;
    grid-template-columns: 10px minmax(0, 1fr) auto;
    align-items: start;
    gap: 0.5rem;
    text-align: left;
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(255, 255, 255, 0.06);
    border-radius: 8px;
    padding: 0.42rem 0.55rem;
    box-shadow: none;
  }

  .entity-row.active {
    border-color: rgba(110, 231, 255, 0.58);
    background: rgba(110, 231, 255, 0.1);
  }

  .entity-kind-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    margin-top: 0.25rem;
    flex-shrink: 0;
  }

  .entity-row-main {
    display: grid;
    min-width: 0;
    gap: 0.12rem;
  }

  .entity-row-main strong,
  .entity-row-main span,
  .entity-row-main em {
    min-width: 0;
    overflow: hidden;
    display: -webkit-box;
    -webkit-box-orient: vertical;
  }

  .entity-row-main strong {
    font-size: 0.82rem;
    -webkit-line-clamp: 1;
    line-clamp: 1;
  }

  .entity-row-main span {
    color: var(--muted);
    font-size: 0.72rem;
    line-height: 1.35;
    -webkit-line-clamp: 1;
    line-clamp: 1;
  }

  .entity-row-main em {
    color: rgba(110, 231, 255, 0.68);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.66rem;
    font-style: normal;
    line-height: 1.3;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    overflow-wrap: anywhere;
  }

  .entity-rel-count {
    color: rgba(255, 255, 255, 0.42);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.68rem;
  }

  .entity-empty {
    margin: 0.4rem 0;
    color: var(--muted);
    font-size: 0.82rem;
    text-align: center;
  }

  .tool-list {
    display: grid;
    gap: 0.4rem;
    align-content: start;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overflow-x: hidden;
  }

  .tool-item {
    width: 100%;
    text-align: left;
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(255, 255, 255, 0.06);
    border-radius: 8px;
    padding: 0.58rem 0.68rem;
    box-shadow: none;
    display: grid;
    gap: 0.25rem;
    font-size: 0.9rem;
    min-width: 0;
  }

  .tool-item-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 0.5rem;
    min-width: 0;
  }

  .tool-item strong {
    font-size: 0.88rem;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .tool-item em {
    flex-shrink: 0;
    color: rgba(110, 231, 255, 0.68);
    font-style: normal;
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.62rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .tool-item-desc {
    color: var(--muted);
    font-size: 0.78rem;
    line-height: 1.35;
    overflow: hidden;
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
  }

  .tool-item.active {
    border-color: rgba(110, 231, 255, 0.45);
    background: linear-gradient(
      135deg,
      rgba(21, 45, 62, 0.95),
      rgba(11, 24, 37, 0.95)
    );
  }

  .qa-btn {
    background: rgba(4, 15, 24, 0.7);
    color: var(--text);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    padding: 0.42rem 0.65rem;
    font-size: 0.78rem;
    box-shadow: none;
    white-space: nowrap;
    flex: 0 0 auto;
  }

  .qa-btn:hover:not(:disabled) {
    border-color: rgba(110, 231, 255, 0.35);
  }

  /* Ingest status block */
  .ingest-status {
    margin-top: 0.75rem;
    padding: 0.65rem 0.8rem;
    border-radius: 14px;
    background: rgba(2, 8, 14, 0.7);
    border: 1px solid rgba(137, 240, 167, 0.25);
  }

  .ingest-status.done {
    border-color: rgba(137, 240, 167, 0.45);
  }

  .ingest-status.error {
    border-color: rgba(244, 132, 95, 0.4);
  }

  .ingest-status.cancelled {
    border-color: rgba(180, 120, 255, 0.4);
  }

  .ingest-status-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }

  .ingest-label {
    font-size: 0.82rem;
    font-weight: 600;
    color: #89f0a7;
    flex: 1;
  }

  .ingest-status.error .ingest-label {
    color: var(--coral);
  }

  .ingest-status.cancelled .ingest-label {
    color: #b478ff;
  }

  .ingest-icon.cancelled {
    color: #b478ff;
  }

  .ingest-icon {
    font-size: 0.9rem;
  }

  .ingest-icon.ok {
    color: #89f0a7;
  }
  .ingest-icon.err {
    color: var(--coral);
  }

  .ingest-dismiss {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 1rem;
    padding: 0;
    cursor: pointer;
    line-height: 1;
    box-shadow: none;
  }

  .ingest-msg {
    margin: 0.35rem 0 0;
    font-size: 0.75rem;
    font-family: "Cascadia Code", "Fira Code", monospace;
    color: var(--muted);
    white-space: pre-wrap;
    word-break: break-all;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .spinner {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid rgba(137, 240, 167, 0.3);
    border-top-color: #89f0a7;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
    flex-shrink: 0;
  }

  .tool-name {
    color: var(--cyan);
    overflow-wrap: anywhere;
  }

  .btn-run {
    background: linear-gradient(135deg, var(--cyan), #b7fff5);
    color: var(--bg);
    padding: 0.45rem 1rem;
    font-size: 0.9rem;
    border-radius: 12px;
  }

  .json-editor {
    width: 100%;
    min-height: 0;
    flex: 1;
    border-radius: 14px;
    background: rgba(2, 8, 14, 0.88);
    border: 1px solid rgba(255, 255, 255, 0.07);
    color: var(--text);
    padding: 0.75rem;
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.85rem;
    resize: none;
  }

  .result-pane {
    min-height: 0;
    flex: 1;
    border-radius: 14px;
    background: rgba(2, 8, 14, 0.88);
    border: 1px solid rgba(255, 255, 255, 0.07);
    padding: 0.75rem;
    overflow: auto;
    white-space: pre-wrap;
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.82rem;
    color: #dce9f8;
    margin: 0;
  }

  .result-head {
    gap: 0.75rem;
  }

  .result-head h2 {
    min-width: 0;
  }

  .result-tool-name {
    color: var(--cyan);
    margin-left: 0.4rem;
    overflow-wrap: anywhere;
  }

  .result-actions {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
    justify-content: flex-end;
  }

  .result-actions span {
    color: var(--muted);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.68rem;
  }

  .result-actions span.success {
    color: var(--success);
  }

  .result-actions span.error {
    color: var(--coral);
  }

  .result-actions button {
    border: 1px solid rgba(110, 231, 255, 0.16);
    border-radius: 8px;
    background: rgba(4, 15, 24, 0.7);
    color: var(--muted);
    box-shadow: none;
    font-size: 0.72rem;
    padding: 0.26rem 0.48rem;
  }

  .result-actions button.active {
    color: var(--cyan);
    background: rgba(110, 231, 255, 0.1);
  }

  .result-empty,
  .result-summary {
    min-height: 0;
    flex: 1;
    border-radius: 14px;
    background: rgba(2, 8, 14, 0.72);
    border: 1px solid rgba(255, 255, 255, 0.07);
    padding: 0.85rem;
    overflow: auto;
  }

  .result-empty {
    display: grid;
    place-items: center;
    color: var(--muted);
    font-size: 0.88rem;
    text-align: center;
  }

  .result-summary {
    display: flex;
    flex-direction: column;
    gap: 0.8rem;
  }

  .result-metrics {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 0.5rem;
  }

  .result-metric {
    min-width: 0;
    border: 1px solid rgba(110, 231, 255, 0.12);
    border-radius: 8px;
    background: rgba(110, 231, 255, 0.055);
    padding: 0.55rem 0.65rem;
  }

  .result-metric span {
    display: block;
    color: var(--muted);
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .result-metric strong {
    display: block;
    margin-top: 0.18rem;
    color: var(--text);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.92rem;
    overflow-wrap: anywhere;
  }

  .result-summary-lists {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 0.65rem;
  }

  .result-summary-lists section {
    min-width: 0;
    border-top: 1px solid rgba(255, 255, 255, 0.07);
    padding-top: 0.55rem;
  }

  .result-summary-lists h3 {
    margin: 0 0 0.45rem;
    color: var(--muted);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
  }

  .result-summary-lists ul {
    margin: 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 0.3rem;
  }

  .result-summary-lists li {
    color: #dce9f8;
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.78rem;
    overflow-wrap: anywhere;
  }

  .entity-workspace-head {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 1rem;
    align-items: start;
    flex-shrink: 0;
  }

  .entity-heading {
    min-width: 0;
  }

  .entity-heading h2 {
    margin: 0.18rem 0 0;
    font-size: 1.35rem;
    line-height: 1.18;
    overflow-wrap: anywhere;
  }

  .entity-heading p {
    margin: 0.35rem 0 0;
    color: var(--muted);
    font-size: 0.88rem;
    line-height: 1.45;
    overflow-wrap: anywhere;
  }

  .entity-primary-actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.4rem;
  }

  .entity-primary-actions button,
  .relation-head button {
    border: 1px solid rgba(110, 231, 255, 0.18);
    border-radius: 8px;
    background: rgba(110, 231, 255, 0.08);
    color: var(--cyan);
    box-shadow: none;
    font-size: 0.76rem;
    padding: 0.34rem 0.55rem;
  }

  .workspace-empty {
    display: grid;
    gap: 0.35rem;
    place-items: center;
    min-height: 112px;
    text-align: center;
    border: 1px dashed rgba(110, 231, 255, 0.18);
    border-radius: 10px;
    background: rgba(2, 8, 14, 0.38);
    flex-shrink: 0;
  }

  .workspace-empty h2 {
    margin: 0;
    color: var(--text);
  }

  .workspace-empty p,
  .soft-empty {
    margin: 0;
    color: var(--muted);
    font-size: 0.88rem;
    line-height: 1.5;
    text-align: center;
  }

  .workspace-facts {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 0.45rem;
    margin: 0.8rem 0 0;
    flex-shrink: 0;
  }

  .workspace-facts div,
  .summary-card {
    min-width: 0;
    border: 1px solid rgba(110, 231, 255, 0.11);
    border-radius: 8px;
    background: rgba(2, 8, 14, 0.5);
    padding: 0.55rem 0.65rem;
  }

  .workspace-facts dt,
  .summary-card span {
    color: rgba(255, 255, 255, 0.42);
    font-size: 0.67rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .workspace-facts dd {
    margin: 0.18rem 0 0;
    color: var(--text);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.75rem;
    overflow-wrap: anywhere;
  }

  .workspace-nav-groups {
    display: flex;
    align-items: end;
    gap: 0.9rem;
    margin-top: 0;
    padding-bottom: 0.55rem;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    flex-wrap: wrap;
    flex-shrink: 0;
  }

  .workspace-nav-groups + .entity-workspace-head,
  .workspace-nav-groups + .workspace-empty {
    margin-top: 0.95rem;
  }

  .workspace-tab-group {
    display: grid;
    gap: 0.28rem;
  }

  .workspace-tab-group > span {
    color: rgba(255, 255, 255, 0.38);
    font-size: 0.66rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .workspace-tabs {
    display: flex;
    gap: 0.35rem;
    overflow-x: auto;
    flex-shrink: 0;
  }

  .workspace-tabs button {
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 8px;
    background: rgba(4, 15, 24, 0.68);
    color: var(--muted);
    box-shadow: none;
    padding: 0.38rem 0.62rem;
    font-size: 0.76rem;
    text-transform: capitalize;
    white-space: nowrap;
  }

  .workspace-tabs button.active {
    color: var(--cyan);
    border-color: rgba(110, 231, 255, 0.38);
    background: rgba(110, 231, 255, 0.1);
  }

  .workspace-tabs button:disabled {
    opacity: 0.42;
    cursor: default;
  }

  .workspace-tab-body {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    min-height: 0;
    flex: 1;
    overflow: auto;
    padding-top: 0.75rem;
  }

  .overview-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 0.55rem;
  }

  .summary-card {
    display: grid;
    gap: 0.24rem;
  }

  .summary-card strong {
    color: var(--text);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 1.05rem;
  }

  .summary-card p {
    margin: 0;
    color: var(--muted);
    font-size: 0.74rem;
    line-height: 1.4;
    overflow-wrap: anywhere;
  }

  .workspace-relation-panel {
    min-height: 0;
  }

  .relationship-workspace {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 280px;
    gap: 0.75rem;
    min-height: 0;
    flex: 1;
  }

  .relationship-table-wrap {
    min-width: 0;
    min-height: 0;
    overflow: auto;
    border: 1px solid rgba(255, 255, 255, 0.06);
    border-radius: 10px;
    background: rgba(2, 8, 14, 0.45);
  }

  .relationship-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.78rem;
  }

  .relationship-table th,
  .relationship-table td {
    padding: 0.55rem 0.6rem;
    border-bottom: 1px solid rgba(255, 255, 255, 0.055);
    color: var(--muted);
    text-align: left;
    vertical-align: top;
  }

  .relationship-table th {
    position: sticky;
    top: 0;
    z-index: 1;
    background: rgba(4, 15, 24, 0.96);
    color: rgba(255, 255, 255, 0.48);
    font-size: 0.66rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .relationship-table tr.active td {
    background: rgba(110, 231, 255, 0.07);
  }

  .table-entity-button {
    display: grid;
    gap: 0.15rem;
    width: 100%;
    text-align: left;
    padding: 0;
    border: 0;
    background: transparent;
    color: var(--text);
    box-shadow: none;
  }

  .table-entity-button strong {
    overflow-wrap: anywhere;
  }

  .table-entity-button span {
    color: var(--muted);
    font-size: 0.72rem;
    overflow-wrap: anywhere;
  }

  .why-panel {
    min-width: 0;
    border: 1px solid rgba(110, 231, 255, 0.12);
    border-radius: 10px;
    background: rgba(2, 8, 14, 0.58);
    padding: 0.8rem;
    overflow: auto;
  }

  .why-panel h3 {
    margin: 0 0 0.55rem;
    color: var(--muted);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
  }

  .why-panel p {
    color: var(--muted);
    font-size: 0.84rem;
    line-height: 1.5;
  }

  .why-panel .entity-facts {
    margin-bottom: 0.85rem;
  }

  .why-panel .entity-facts div {
    grid-template-columns: 72px minmax(0, 1fr);
  }

  .why-panel .entity-facts dd {
    min-width: 0;
    white-space: normal;
    overflow-wrap: anywhere;
  }

  .lane-list {
    display: grid;
    gap: 0.45rem;
    align-content: start;
  }

  .lane-row {
    display: grid;
    gap: 0.2rem;
    text-align: left;
    border: 1px solid rgba(255, 255, 255, 0.06);
    border-radius: 8px;
    background: rgba(2, 8, 14, 0.55);
    color: var(--text);
    box-shadow: none;
    padding: 0.62rem 0.7rem;
  }

  .lane-row span {
    color: var(--muted);
    font-size: 0.75rem;
    overflow-wrap: anywhere;
  }

  .graph-workspace {
    display: flex;
    flex-direction: column;
    min-height: 620px;
    flex: 1;
  }

  .graph-workspace.inactive {
    display: none;
  }

  .advanced-workspace {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    min-height: 620px;
    flex: 1;
    overflow: hidden;
  }

  .tool-desc,
  .section-gap {
    flex-shrink: 0;
  }

  .tool-form {
    min-height: 0;
    max-height: 38%;
    overflow-y: auto;
    overflow-x: hidden;
    padding-right: 0.2rem;
  }

  .graph-surface {
    height: auto;
    min-height: 300px;
    flex: 1 1 48%;
    border-radius: 18px;
    overflow: hidden;
    border: 1px solid rgba(110, 231, 255, 0.12);
    background: #07111b;
    flex-shrink: 0;
    cursor: grab;
    touch-action: none;
  }

  .graph-surface:active {
    cursor: grabbing;
  }

  .graph-surface :global(canvas) {
    display: block;
    cursor: inherit;
  }

  /* Graph filter */
  .graph-filter-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.5rem;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
    flex-shrink: 0;
  }

  .graph-filter {
    flex: 1;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(110, 231, 255, 0.15);
    border-radius: 8px;
    color: var(--text);
    font-size: 0.82rem;
    padding: 0.3rem 0.6rem;
    outline: none;
  }

  .graph-filter:focus {
    border-color: rgba(110, 231, 255, 0.4);
    background: rgba(110, 231, 255, 0.06);
  }

  .graph-filter-clear {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 1rem;
    cursor: pointer;
    padding: 0 0.25rem;
    line-height: 1;
  }

  .graph-filter-clear:hover {
    color: var(--text);
  }

  .graph-filter-load {
    background: rgba(110, 231, 255, 0.1);
    border: 1px solid rgba(110, 231, 255, 0.2);
    border-radius: 6px;
    color: var(--cyan);
    font-size: 0.9rem;
    cursor: pointer;
    padding: 0.2rem 0.4rem;
    line-height: 1;
    flex-shrink: 0;
  }

  .graph-filter-load:hover:not(:disabled) {
    background: rgba(110, 231, 255, 0.18);
  }
  .graph-filter-load:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .snapshot-controls {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0 0.5rem 0.5rem;
    flex-wrap: wrap;
    flex-shrink: 0;
  }

  .snapshot-mode {
    display: inline-flex;
    border: 1px solid rgba(110, 231, 255, 0.16);
    border-radius: 8px;
    overflow: hidden;
    background: rgba(2, 8, 14, 0.55);
  }

  .snapshot-mode button,
  .snapshot-refresh,
  .layout-toggle {
    background: transparent;
    border: 0;
    box-shadow: none;
    color: var(--muted);
    font-size: 0.76rem;
    padding: 0.25rem 0.55rem;
    line-height: 1.2;
  }

  .snapshot-mode button.active {
    background: rgba(110, 231, 255, 0.14);
    color: var(--cyan);
  }

  .snapshot-limit {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    color: var(--muted);
    font-size: 0.72rem;
  }

  .snapshot-limit input {
    width: 72px;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(110, 231, 255, 0.14);
    border-radius: 7px;
    color: var(--text);
    font-size: 0.76rem;
    padding: 0.22rem 0.4rem;
  }

  .snapshot-refresh,
  .layout-toggle {
    border: 1px solid rgba(110, 231, 255, 0.16);
    border-radius: 7px;
    color: var(--cyan);
    cursor: pointer;
  }

  .layout-toggle.active {
    background: rgba(110, 231, 255, 0.14);
  }

  .snapshot-refresh:hover:not(:disabled),
  .snapshot-mode button:hover,
  .layout-toggle:hover {
    background: rgba(110, 231, 255, 0.09);
  }

  .layout-controls {
    display: grid;
    gap: 0.35rem;
    padding: 0 0.5rem 0.6rem;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
    flex-shrink: 0;
  }

  .edge-layout-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 0.25rem 0.55rem;
  }

  .layout-slider {
    display: grid;
    grid-template-columns: minmax(68px, 1fr) minmax(72px, 1.1fr) 1.8rem;
    align-items: center;
    gap: 0.35rem;
    color: var(--muted);
    font-size: 0.68rem;
  }

  .layout-slider-wide {
    grid-template-columns: minmax(96px, 1fr) minmax(150px, 2fr) 1.8rem;
  }

  .layout-slider span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .layout-slider input[type="range"] {
    width: 100%;
    accent-color: var(--cyan);
  }

  .layout-slider output {
    color: rgba(255, 255, 255, 0.45);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.7rem;
    text-align: right;
  }

  .graph-mode-badge {
    font-size: 0.72rem;
    font-weight: 400;
    border-radius: 6px;
    padding: 0.15rem 0.45rem;
    margin-left: 0.5rem;
    vertical-align: middle;
  }

  .graph-mode-badge.global {
    background: rgba(255, 255, 255, 0.06);
    color: var(--muted);
  }

  .graph-mode-badge.focused {
    background: rgba(110, 231, 255, 0.12);
    color: var(--cyan);
    font-family: monospace;
  }

  /* Legend */
  .graph-legend {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem 0.85rem;
    padding: 0.55rem 0.25rem;
    border-top: 1px solid rgba(255, 255, 255, 0.05);
    margin-top: 0.5rem;
    flex-shrink: 0;
    max-height: 92px;
    overflow-y: auto;
    overflow-x: hidden;
  }

  .legend-section {
    font-size: 0.65rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: rgba(255, 255, 255, 0.25);
    align-self: center;
  }

  .legend-sep {
    width: 1px;
    height: 14px;
    background: rgba(255, 255, 255, 0.1);
    flex-shrink: 0;
  }

  .legend-item {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.75rem;
    color: var(--muted);
  }

  .legend-toggle {
    cursor: pointer;
  }

  .legend-toggle input {
    width: 12px;
    height: 12px;
    margin: 0;
    accent-color: var(--cyan);
  }

  .legend-toggle.muted {
    opacity: 0.48;
  }

  .legend-count {
    color: rgba(255, 255, 255, 0.38);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.68rem;
  }

  .legend-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .legend-line {
    width: 20px;
    height: 2px;
    border-radius: 2px;
    flex-shrink: 0;
  }

  .legend-line.crossfile {
    background: #ffffff;
    box-shadow: 0 0 4px #fff;
  }

  .node-kind {
    margin: 0;
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
  }

  .entity-facts {
    display: grid;
    gap: 0.35rem;
    margin: 0.75rem 0 0;
  }

  .entity-facts div {
    display: grid;
    grid-template-columns: 48px minmax(0, 1fr);
    gap: 0.5rem;
    align-items: baseline;
  }

  .entity-facts dt {
    color: rgba(255, 255, 255, 0.38);
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .entity-facts dd {
    margin: 0;
    color: var(--muted);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.72rem;
    overflow-wrap: anywhere;
  }

  .relation-panel {
    margin-top: 0.85rem;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    padding-top: 0.75rem;
  }

  .relation-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 0.45rem;
  }

  .relation-head h3 {
    margin: 0;
    color: var(--muted);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
  }

  .relation-list {
    display: grid;
    gap: 0.3rem;
    max-height: 220px;
    overflow: auto;
  }

  .relation-row {
    display: grid;
    grid-template-columns: minmax(82px, 0.7fr) minmax(0, 1fr) 38px;
    gap: 0.45rem;
    align-items: center;
    text-align: left;
    padding: 0.42rem 0.5rem;
    background: rgba(255, 255, 255, 0.035);
    border: 1px solid rgba(255, 255, 255, 0.055);
    border-radius: 8px;
    color: var(--text);
    box-shadow: none;
  }

  .relation-row:hover {
    border-color: rgba(110, 231, 255, 0.3);
  }

  .relation-type,
  .relation-meta {
    color: rgba(255, 255, 255, 0.42);
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.66rem;
  }

  .relation-label {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.76rem;
  }

  .relation-meta {
    text-align: right;
  }

  .relation-empty {
    margin: 0.35rem 0;
    color: var(--muted);
    font-size: 0.78rem;
    text-align: center;
  }

  .node-actions {
    margin-top: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }

  .retract-reason {
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 6px;
    color: var(--fg);
    font-size: 0.82rem;
    padding: 0.3rem 0.5rem;
    width: 100%;
    box-sizing: border-box;
  }

  .btn-retract {
    background: rgba(220, 60, 60, 0.18);
    border: 1px solid rgba(220, 60, 60, 0.45);
    border-radius: 6px;
    color: #f08080;
    cursor: pointer;
    font-size: 0.82rem;
    padding: 0.3rem 0.7rem;
    transition: background 0.15s;
  }

  .btn-retract:hover:not(:disabled) {
    background: rgba(220, 60, 60, 0.35);
  }

  .btn-retract:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .retract-error {
    margin: 0;
    color: #f08080;
    font-size: 0.78rem;
  }

  @media (max-width: 1200px) {
    :global(body:has(.workspace-grid)) {
      overflow: auto;
    }

    .workspace-grid {
      grid-template-columns: 1fr;
      height: auto;
      overflow: visible;
    }

    .tool-panel,
    .workspace-panel {
      max-height: none;
      height: auto;
    }

    .entity-list,
    .tool-list {
      max-height: 520px;
    }

    .graph-surface {
      height: 400px;
      flex: none;
    }

    .entity-workspace-head,
    .relationship-workspace,
    .workspace-facts,
    .overview-grid {
      grid-template-columns: 1fr;
    }

    .entity-primary-actions {
      justify-content: flex-start;
    }
  }

  /* ---- Command panel form ---- */

  .cmd-actions {
    display: flex;
    gap: 0.4rem;
    align-items: center;
  }

  .btn-mode {
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 8px;
    color: var(--muted);
    font-size: 0.78rem;
    padding: 0.3rem 0.65rem;
    cursor: pointer;
    box-shadow: none;
    white-space: nowrap;
  }

  .btn-mode:hover {
    color: var(--text);
    border-color: rgba(255, 255, 255, 0.2);
  }

  .tool-desc {
    font-size: 0.8rem;
    color: var(--muted);
    margin: 0 0 0.5rem;
    line-height: 1.5;
  }

  .tool-form {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    overflow-y: auto;
    max-height: 38%;
    padding-right: 2px;
  }

  .form-repo-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.3rem 0.6rem;
    background: rgba(255, 255, 255, 0.03);
    border-radius: 8px;
    border: 1px solid rgba(255, 255, 255, 0.05);
  }

  .form-repo-tag {
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
    flex-shrink: 0;
  }

  .form-repo-path {
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.77rem;
    color: var(--cyan);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .form-field {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }

  .form-field-head {
    display: flex;
    align-items: center;
    gap: 0.45rem;
  }

  .form-label {
    font-size: 0.82rem;
    font-weight: 600;
    color: var(--text);
    font-family: "Cascadia Code", "Fira Code", monospace;
  }

  .form-req {
    font-size: 0.68rem;
    color: var(--coral);
    background: rgba(244, 132, 95, 0.12);
    padding: 0.1rem 0.38rem;
    border-radius: 4px;
    letter-spacing: 0.04em;
  }

  .form-hint {
    font-size: 0.75rem;
    color: var(--muted);
    margin: 0;
    line-height: 1.4;
  }

  .form-input,
  .form-select {
    background: rgba(2, 8, 14, 0.88);
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 8px;
    color: var(--text);
    padding: 0.42rem 0.65rem;
    font-size: 0.85rem;
    font-family: "Cascadia Code", "Fira Code", monospace;
    width: 100%;
    box-sizing: border-box;
  }

  .form-input:focus,
  .form-select:focus {
    border-color: rgba(110, 231, 255, 0.4);
    outline: none;
    background: rgba(110, 231, 255, 0.04);
  }

  .form-select {
    -webkit-appearance: none;
    appearance: none;
    cursor: pointer;
  }

  .form-textarea {
    background: rgba(2, 8, 14, 0.88);
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 8px;
    color: var(--text);
    padding: 0.42rem 0.65rem;
    font-size: 0.85rem;
    font-family: "Cascadia Code", "Fira Code", monospace;
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    min-height: 70px;
  }

  .form-textarea:focus {
    border-color: rgba(110, 231, 255, 0.4);
    outline: none;
    background: rgba(110, 231, 255, 0.04);
  }

  .form-toggle {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    user-select: none;
  }

  .form-toggle input[type="checkbox"] {
    display: none;
  }

  .toggle-track {
    width: 36px;
    height: 20px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.08);
    border: 1px solid rgba(255, 255, 255, 0.12);
    position: relative;
    transition:
      background 0.18s,
      border-color 0.18s;
    flex-shrink: 0;
  }

  .form-toggle input[type="checkbox"]:checked + .toggle-track {
    background: rgba(110, 231, 255, 0.2);
    border-color: rgba(110, 231, 255, 0.4);
  }

  .toggle-thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--muted);
    transition:
      transform 0.18s,
      background 0.18s;
  }

  .form-toggle input[type="checkbox"]:checked + .toggle-track .toggle-thumb {
    transform: translateX(16px);
    background: var(--cyan);
  }

  .toggle-label {
    font-size: 0.8rem;
    color: var(--muted);
  }

  .form-empty {
    font-size: 0.82rem;
    color: var(--muted);
    text-align: center;
    padding: 1.25rem 0;
  }
</style>
