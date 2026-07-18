export type Project = {
    projectId: string;
    repositoryRoot: string;
    registeredAt: string;
};
export type Agent = {
    agentId: string;
    projectId: string;
    missionId: string | null;
    directionId: string | null;
    autoresearchId: string | null;
    experimentId: string | null;
    runId: string | null;
    jobId: string | null;
    role: string;
    status: string;
    currentTool: string | null;
    startedAt: string;
    lastEventAt: string;
    taskId: string | null;
    modelProvider: string | null;
    modelId: string | null;
    modelName: string | null;
    thinkingLevel: string;
    contextTokens: number | null;
    contextWindow: number | null;
    contextPercent: number | null;
};
export type Job = {
    jobId: string;
    experimentId: string;
    state: string;
    startedAt: string | null;
    command: string[];
};
export type NoshEvent = {
    eventId: string;
    sequence: number | null;
    timestamp: string;
    type: string;
    source: string;
    payload: unknown;
    scope: {
        projectId: string;
        missionId: string | null;
        directionId: string | null;
        autoresearchId: string | null;
        experimentId: string | null;
        runId: string | null;
        agentId: string | null;
        jobId: string | null;
    };
};
export type GraphNodeRecord = {
    id: string;
    type: string;
    title: string;
    required: boolean;
    hardDependencies: string[];
    state: string;
    attempt: number;
    maximumAttempts: number;
};
export type Stored<T> = {
    entityId: string;
    version: number;
    state: string;
    updatedAt: string;
    value: T;
};
export type MissionRecord = {
    missionId: string;
    projectId: string;
    title: string;
    objective: string;
    deliverables: string[];
    successCriteria: string[];
    approvedGraphVersion: number | null;
    state: string;
    graphVersion: number;
    nodes: GraphNodeRecord[];
    createdAt: string;
    updatedAt: string;
};
export type DirectionRecord = {
    directionId: string;
    projectId: string;
    missionId: string | null;
    question: string;
    decisionUse: string;
    state: string;
    evaluationContractHash: string;
    acceptedBaseline: {
        commit: string;
        reviewId: string;
    } | null;
    graphVersion: number;
    nodes: GraphNodeRecord[];
    createdAt: string;
    updatedAt: string;
};
export type AutoresearchRecord = {
    autoresearchId: string;
    projectId: string;
    directionId: string | null;
    decisionQuestion: string;
    familyTags: string[];
    scope: string[];
    state: string;
    evaluationContractHash: string;
    currentRound: number;
    maximumExperiments: number;
    maximumRounds: number;
    maximumWallClockSeconds: number;
    maximumModelTokens: number;
    maximumGpuSeconds: number;
    maximumDiskBytes: number;
};
export type ThreadRecord = {
    threadId: string;
    projectId: string;
    purpose: string;
    executionMode: "background" | "foreground_fork";
    state: string;
    parentThreadId: string | null;
    childThreadIds: string[];
    episodeIds: string[];
    role: string;
    currentAgentId: string | null;
    sessionHistory: Array<{
        agentId: string;
        piSessionId: string;
        startedAt: string;
        endedAt: string | null;
        endReason: string | null;
    }>;
    usage: {
        toolCalls: number;
        modelTokens: number;
        wallClockSeconds: number;
    };
    budget: {
        maximumToolCalls: number;
        maximumModelTokens: number;
        maximumWallClockSeconds: number;
    };
    ownerScope: {
        missionId: string | null;
        directionId: string | null;
        autoresearchId: string | null;
        experimentId: string | null;
        graphNodeId: string | null;
    };
    updatedAt: string;
};
export type EpisodeRecord = {
    episodeId: string;
    threadId: string;
    instructionId: string;
    stepNumber: number;
    episodeType: string;
    objective: string;
    status: string;
    summary: string;
    facts: Array<{
        statement: string;
        evidenceRefs: string[];
        confidence: string;
    }>;
    decisions: Array<{
        statement: string;
        rationale: string;
        evidenceRefs: string[];
    }>;
    artifactIds: string[];
    evidenceIds: string[];
    changedFiles: string[];
    unresolvedQuestions: string[];
    trace: {
        firstSequence: number;
        lastSequence: number;
    };
    usage: {
        toolCalls: number;
        modelTokens: number;
        wallClockSeconds: number;
    };
    episodeHash: string;
    completedAt: string;
};
export declare function token(): string;
export declare function api<T>(path: string, init?: RequestInit): Promise<T>;
export declare function subscribe(projectId: string, after: number, receive: (event: NoshEvent) => void): () => void;
