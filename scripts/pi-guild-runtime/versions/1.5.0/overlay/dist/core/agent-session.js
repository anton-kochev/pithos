/**
 * AgentSession - Core abstraction for agent lifecycle and session management.
 *
 * This class is shared between all run modes (interactive, print, rpc).
 * It encapsulates:
 * - Agent state access
 * - Event subscription with automatic session persistence
 * - Model and thinking level management
 * - Compaction (manual and auto)
 * - Bash execution
 * - Session switching and branching
 *
 * Modes use this class and add their own I/O layer on top.
 */
import { AsyncLocalStorage } from "node:async_hooks";
import { readFileSync } from "node:fs";
import { basename, dirname } from "node:path";
import { runToolCall, } from "@earendil-works/pi-agent-core";
import { contentText, getCurrentSystemMessage, getCurrentTools, retryDelayMs } from "@earendil-works/pi-ai";
import { clampThinkingLevel, cleanupSessionResources, getSupportedThinkingLevels, isContextOverflow, isRecoverableLength, isRetryableAssistantError, modelsAreEqual, resetApiProviders, streamSimple, } from "@earendil-works/pi-ai/compat";
import { getThemeByName, theme } from "../modes/interactive/theme/theme.js";
import { stripFrontmatter } from "../utils/frontmatter.js";
import { processImage } from "../utils/image-process.js";
import { sleep } from "../utils/sleep.js";
import { normalizeToolResultImages } from "../utils/tool-result-images.js";
import { formatNoApiKeyFoundMessage, formatNoModelSelectedMessage } from "./auth-guidance.js";
import { executeBashWithOperations } from "./bash-executor.js";
import { generateBugReportSummary } from "./bug-report.js";
import { calculateContextTokens, collectEntriesForBranchSummary, compact, estimateContextTokens, estimateProjectedContextTokens, estimateTokens, generateBranchSummary, prepareCompaction, shouldCompact, } from "./compaction/index.js";
import { DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS } from "./defaults.js";
import { exportSessionToHtml } from "./export-html/index.js";
import { createToolHtmlRenderer } from "./export-html/tool-renderer.js";
import { bindAtomicOwnedToolsBackend } from "./extensions/atomic-owned-tools.js";
import { ExtensionRunner, wrapRegisteredTools, } from "./extensions/index.js";
import { emitSessionShutdownEvent } from "./extensions/runner.js";
import { bindToolRegistryWriter } from "./extensions/tool-registry-write.js";
import { createToolNameMatcher, isMcpToolName } from "./mcp-servers.js";
import { convertToLlm } from "./messages.js";
import { ModelRegistry } from "./model-registry.js";
import { NestedToolCallRunner } from "./nested-tool-calls.js";
import { createContinuationGate, createParentContinuation, } from "./parent-continuation.js";
import { expandPromptTemplate } from "./prompt-templates.js";
import { exportSessionToJsonl } from "./session-export.js";
import { getLatestCompactionEntry, SessionManager, } from "./session-manager.js";
import { applyToolModifiers, DEFAULT_TOOL_NAMES, } from "./settings-manager.js";
import { BUILTIN_PATH_PREFIX, createSyntheticSourceInfo, isSyntheticPath } from "./source-info.js";
import { buildSystemPrompt, buildSystemPromptSections, diffSystemPromptSections, normalizeBuildSystemPromptOptions, } from "./system-prompt.js";
import { createLocalBashOperations } from "./tools/bash.js";
import { createAllToolDefinitions } from "./tools/index.js";
import { createToolDefinitionFromAgentTool } from "./tools/tool-definition-wrapper.js";
import { addUsageToTotals, combineUsage, createUsageTotals } from "./usage-totals.js";
import { findLatestResponse, getBranchSelection, getVirtualModelState, isVirtualModel, VIRTUAL_MODEL_STATE_ENTRY, } from "./virtual-models.js";
/**
 * Stream functions that resolve auth through a given ModelRuntime themselves, such as the one
 * createAgentSession installs. Summaries sent through them must not pass pre-resolved credentials:
 * the runtime would take them as an explicit API key instead of the stored OAuth login (#11520).
 */
const runtimeStreamFunctions = new WeakMap();
/** Mark `streamFn` as resolving auth through `modelRuntime` on its own. */
export function markRuntimeStreamFunction(streamFn, modelRuntime) {
    runtimeStreamFunctions.set(streamFn, modelRuntime);
}
/**
 * Parse a skill block from message text.
 * Returns null if the text doesn't contain a skill block.
 */
export function parseSkillBlock(text) {
    const match = text.match(/^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n<\/skill>(?:\n\n([\s\S]+))?$/);
    if (!match)
        return null;
    return {
        name: match[1],
        location: match[2],
        content: match[3],
        userMessage: match[4]?.trim() || undefined,
    };
}
// ============================================================================
// Types
// ============================================================================
function withoutDeletedHeaders(headers) {
    return headers
        ? Object.fromEntries(Object.entries(headers).filter((entry) => entry[1] !== null))
        : undefined;
}
function estimateMessagesTokens(messages) {
    let tokens = 0;
    for (const message of messages) {
        tokens += estimateTokens(message);
    }
    return tokens;
}
// ============================================================================
// AgentSession Class
// ============================================================================
export class AgentSession {
    agent;
    sessionManager;
    settingsManager;
    _scopedModels;
    // Event subscription state
    _unsubscribeAgent;
    _eventListeners = [];
    _isAgentRunActive = false;
    _commandGeneration = {};
    _commandGateMember = {
        beginControl: () => this._parentContinuation.beginControl(),
        invalidateAll: () => {
            this._commandGeneration = {};
            this._parentContinuation.invalidateAll();
        },
        revokePending: () => this._parentContinuation.revokePending(),
        notifyIdle: () => this._parentContinuation.notifyIdle(),
    };
    _parentContinuationDisposed = false;
    _continuationGate = createContinuationGate();
    _continuationRuntimeGate;
    _detachContinuationRuntimeGate;
    _detachContinuationGate;
    _parentContinuationToken;
    _isPreparingParentContinuation = false;
    _ownedDispatchActive = false;
    _nativeDrainPump;
    _nativeDrainFailure;
    static _nativeDeferralContext = new AsyncLocalStorage();
    static _nativeDrainContext = new AsyncLocalStorage();
    static _commandTailContext = new AsyncLocalStorage();
    _commandTails = new Set();
    _commandTailFailure;
    _nativeInputDepth = 0;
    _isDrainingNativeActions = false;
    _settlementDepth = 0;
    _parentContinuation = createParentContinuation({
        isCurrent: () => !this._parentContinuationDisposed &&
            this._continuationGate.isAuthorityAvailable() &&
            (this._continuationRuntimeGate?.isAuthorityAvailable() ?? true),
        isAuthorityAvailable: () => this._continuationGate.isAuthorityAvailable() &&
            (this._continuationRuntimeGate?.isAuthorityAvailable() ?? true),
        isEligibleIdle: () => !this._continuationGate.isBusy() &&
            !this._continuationRuntimeGate?.isBusy() &&
            this.isIdle &&
            !this.pendingMessageCount &&
            !this.agent.hasQueuedMessages() &&
            this._nativeInputDepth === 0 &&
            this._settlementDepth === 0 &&
            !this._isPreparingParentContinuation &&
            !this._ownedDispatchActive &&
            !this._nativeDrainPump &&
            !this._isDrainingNativeActions &&
            this._deferredSettledActions.length === 0 &&
            !this.isRetrying,
        onDispatchFinished: () => {
            this._ownedDispatchActive = false;
            // Coordinator has published actual owned cleanup before foreign lifecycle calls can join it.
            this._nativeDrainPump = this._drainNativeActions(false)
                .catch((error) => {
                // An error reporter can itself throw. Retain that failure for the Session join,
                // rather than rejecting a detached task or reporting the same input twice.
                this._nativeDrainFailure ??= { error };
            })
                .finally(() => {
                this._nativeDrainPump = undefined;
                this._resolveIdleWaitIfIdle();
                this._parentContinuation.notifyIdle();
            });
        },
        onIdleCandidate: () => this._parentContinuation.notifyIdle(),
        dispatch: (request, control) => this._startRun({
            messageType: "custom",
            message: {
                role: "custom",
                customType: "pi-parent-continuation",
                content: request.content,
                display: true,
                timestamp: Date.now(),
            },
        }, undefined, { request, control }),
        cancel: (token) => {
            if (this._parentContinuationToken !== token)
                return;
            this._agentRunAbortRequested = true;
            this.abortRetry();
            this.agent.abort();
        },
    });
    _agentRunAbortRequested = false;
    _idleWaitPromise;
    _resolveIdleWait;
    /** Tracks pending steering messages for UI display. Removed when delivered. */
    _steeringMessages = [];
    /** Tracks pending follow-up messages for UI display. Removed when delivered. */
    _followUpMessages = [];
    /** Messages queued to be included with the next user prompt as context ("asides"). */
    _pendingNextTurnMessages = [];
    /** Context-only custom messages queued during a run, flushed once the current turn's tool results are in. */
    _pendingCustomMessages = [];
    // Compaction state
    _compactionAbortController = undefined;
    _autoCompactionAbortController = undefined;
    _overflowRecoveryAttempted = false;
    // Branch summarization state
    _branchSummaryAbortController = undefined;
    // Retry state
    _retryAbortController = undefined;
    _retryAttempt = 0;
    /**
     * Failed response that the next request repeats, set by auto-retry and overflow recovery. The
     * retry is routed with it as `failed`, since the context no longer contains it.
     */
    _failedResponse;
    // Bash execution state
    _bashAbortControllers = new Set();
    _pendingBashMessages = [];
    // Extension system
    _extensionRunner;
    _turnIndex = 0;
    _entryIdsByMessage = new WeakMap();
    _boundaryDispatchedMessages = new WeakSet();
    _lastAssistantMessage;
    _lastActivityOutcome = "completed";
    _isBeforeSettle = false;
    _abortDuringBeforeSettle = false;
    _isEmittingAgentSettled = false;
    _deferredSettledActions = [];
    _resourceLoader;
    _customTools;
    _baseToolDefinitions = new Map();
    _cwd;
    _extensionRunnerRef;
    _initialActiveToolNames;
    /**
     * Tools of the restored or reloaded loadout that are not registered yet, such as tools of MCP
     * servers that are still connecting. They are activated when they are registered, and dropped when
     * `setActiveToolsByName()` deactivates a tool or the next agent run starts.
     */
    _pendingToolNames = new Set();
    _usesDefaultTools;
    _defaultToolModifiers;
    /** Matches the `--tools` entries: tool names or patterns. */
    _allowedTools;
    /**
     * Whether the allowlist filters MCP tools: it is empty (`--no-tools`) or names an MCP tool
     * (`mcp__*`). Otherwise it keeps MCP tools registered for codemode and tool_search.
     */
    _allowlistFiltersMcp = false;
    _excludedTools;
    _baseToolsOverride;
    _sessionStartEvent;
    _extensionUIContext;
    _extensionMode = "print";
    _extensionCommandContextActions;
    _extensionAbortHandler;
    _extensionShutdownHandler;
    _extensionErrorListener;
    _extensionErrorUnsubscriber;
    _modelRuntime;
    _cacheWarmer;
    // Tool registry for extension getTools/setTools
    _toolRegistry = new Map();
    /** Entry-based, not value-based: even a failed or same-value nested write supersedes its caller. */
    _toolSelectionRevision = 0;
    _preparingToolHooks = new Map();
    /** Created on the first `ctx.executeTool()` call. */
    _nestedToolCalls;
    /** Declared tools whose declarations requests leave out, from `prepareLoadout` hooks. */
    _hiddenDeclarations = new Set();
    _toolDefinitions = new Map();
    _toolPromptSnippets = new Map();
    _toolPromptGuidelines = new Map();
    _baseSystemPromptOptions;
    /** Prompt options after before_agent_start mutations for the active run. */
    _runSystemPromptOptions;
    constructor(config) {
        this._detachContinuationGate = this._continuationGate.register(this._commandGateMember);
        if (config.continuationRuntimeGate)
            this.attachContinuationRuntimeGate(config.continuationRuntimeGate);
        this.agent = config.agent;
        this.sessionManager = config.sessionManager;
        this.settingsManager = config.settingsManager;
        this._scopedModels = config.scopedModels ?? [];
        this._resourceLoader = config.resourceLoader;
        this._customTools = config.customTools ?? [];
        this._cwd = config.cwd;
        // Initial preparation can be superseded before any registry is published.
        this._baseSystemPromptOptions = normalizeBuildSystemPromptOptions({ cwd: this._cwd, selectedTools: [] });
        this._modelRuntime = config.modelRuntime;
        this._cacheWarmer = config.cacheWarmer;
        if (this._cacheWarmer) {
            this._cacheWarmer.onWarmed = (entry) => this._emit({ type: "entry_appended", entry });
        }
        this._extensionRunnerRef = config.extensionRunnerRef;
        this._initialActiveToolNames = config.initialActiveToolNames;
        this._usesDefaultTools = config.usesDefaultTools ?? false;
        this._defaultToolModifiers = config.defaultToolModifiers ?? [];
        if (config.allowedToolNames) {
            this._allowedTools = createToolNameMatcher(config.allowedToolNames);
            this._allowlistFiltersMcp =
                config.allowedToolNames.length === 0 || config.allowedToolNames.some((entry) => entry.startsWith("mcp__"));
        }
        this._excludedTools = config.excludedToolNames ? createToolNameMatcher(config.excludedToolNames) : undefined;
        this._baseToolsOverride = config.baseToolsOverride;
        this._sessionStartEvent = config.sessionStartEvent ?? { type: "session_start", reason: "startup" };
        // Always subscribe to agent events for internal handling
        // (session persistence, extensions, auto-compaction, retry logic)
        this._unsubscribeAgent = this.agent.subscribe(this._handleAgentEvent);
        this._installAgentToolHooks();
        this._installAgentNextTurnRefresh();
        this._installAgentRequestProjection();
        this._installAgentBoundaryHooks();
        this._installHiddenDeclarationsProjection();
        this._installAgentForcedPromptProjection();
        this._buildRuntime({
            activeToolNames: this._initialActiveToolNames,
            includeAllExtensionTools: true,
        });
        if (this._usesDefaultTools)
            this._restoreToolsFromTranscript(this._defaultToolModifiers);
    }
    get modelRuntime() {
        return this._modelRuntime;
    }
    async _getRequiredRequestAuth(model, signal) {
        let result;
        try {
            result = await this._modelRuntime.getAuth(model, { signal });
        }
        catch (error) {
            const cause = error instanceof Error ? error.cause : undefined;
            if (cause instanceof Error && cause.message === "authHeader requires a resolved API key") {
                throw new Error(formatNoApiKeyFoundMessage(model.provider));
            }
            throw error;
        }
        if (result && (result.auth.apiKey || result.auth.headers)) {
            const requestModel = result.auth.baseUrl ? { ...model, baseUrl: result.auth.baseUrl } : model;
            return {
                model: requestModel,
                apiKey: result.auth.apiKey,
                headers: withoutDeletedHeaders(result.auth.headers),
                env: result.env,
            };
        }
        const isOAuth = this._modelRuntime.isUsingOAuth(model.provider);
        if (isOAuth) {
            throw new Error(`Authentication failed for "${model.provider}". ` +
                `Credentials may have expired or network is unavailable. ` +
                `Run '/login ${model.provider}' to re-authenticate.`);
        }
        throw new Error(formatNoApiKeyFoundMessage(model.provider));
    }
    async _getSummarizationRequestAuth(selectedModel, signal) {
        // Route a virtual model first: summaries size their input and output from the model they get.
        const { model, thinkingLevel } = isVirtualModel(selectedModel)
            ? await this._modelRuntime.resolveModel(selectedModel, convertToLlm(this.messages), {
                reason: "direct",
                thinkingLevel: this.thinkingLevel,
                signal,
            })
            : { model: selectedModel, thinkingLevel: this.thinkingLevel };
        if (this.agent.streamFunction === streamSimple) {
            return { ...(await this._getRequiredRequestAuth(model, signal)), thinkingLevel };
        }
        if (runtimeStreamFunctions.get(this.agent.streamFunction) === this._modelRuntime) {
            return { model, thinkingLevel };
        }
        try {
            const result = await this._modelRuntime.getAuth(model, { signal });
            if (!result)
                return { model, thinkingLevel };
            const requestModel = result.auth.baseUrl ? { ...model, baseUrl: result.auth.baseUrl } : model;
            return {
                model: requestModel,
                apiKey: result.auth.apiKey,
                headers: withoutDeletedHeaders(result.auth.headers),
                env: result.env,
                thinkingLevel,
            };
        }
        catch (error) {
            if (signal?.aborted)
                throw error;
            return { model, thinkingLevel };
        }
    }
    /**
     * The model whose limits apply to `message`, or undefined when the message came from another
     * model. Under a virtual selection, that is the physical model that produced it.
     */
    _modelForMessage(message) {
        const model = this.model;
        if (model && isVirtualModel(model))
            return this._modelRuntime.getPhysicalModel(message.provider, message.model);
        return model?.provider === message.provider && model.id === message.model ? model : undefined;
    }
    /**
     * Record the selection on the current branch when the branch implies another one, so a resume
     * restores it. Tree navigation can leave the latest `model_change` on another branch; responses
     * cannot record a virtual selection because they name physical models. Responses do record a
     * physical selection unless the branch holds a virtual one; checking a physical selection against
     * responses would record it on every prompt while `prepareRequest` redirects to another model.
     */
    _recordSelection() {
        const model = this.model;
        if (!model)
            return;
        const getModel = (provider, modelId) => this._modelRuntime.getModel(provider, modelId);
        const recorded = getBranchSelection(this.sessionManager.getBranch(), getModel);
        if (!recorded || (recorded.provider === model.provider && recorded.modelId === model.id))
            return;
        const recordedModel = getModel(recorded.provider, recorded.modelId);
        if (!isVirtualModel(model) && !(recordedModel && isVirtualModel(recordedModel)))
            return;
        this.sessionManager.appendModelChange(model.provider, model.id);
    }
    /** The model whose limits apply to the conversation. */
    _limitsModel() {
        return this.routedModel?.model ?? this.model;
    }
    /**
     * Install tool hooks once on the Agent instance.
     *
     * The callbacks read `this._extensionRunner` at execution time, so extension reload swaps in the
     * new runner without reinstalling hooks. Extension-specific tool wrappers are still used to adapt
     * registered tool execution to the extension context. Tool call and tool result interception now
     * happens here instead of in wrappers.
     */
    _installAgentToolHooks() {
        this.agent.beforeToolCall = (context) => this._beforeToolCall(context);
        this.agent.afterToolCall = (context) => this._afterToolCall(context);
    }
    /** `tool_call` handlers. `parentToolCallId` is set for calls another tool made. */
    async _beforeToolCall({ toolCall, args }, parentToolCallId) {
        const runner = this._extensionRunner;
        if (!runner.hasHandlers("tool_call")) {
            return undefined;
        }
        try {
            return await runner.emitToolCall({
                type: "tool_call",
                toolName: toolCall.name,
                toolCallId: toolCall.id,
                ...(parentToolCallId ? { parentToolCallId } : {}),
                input: args,
            });
        }
        catch (err) {
            if (err instanceof Error) {
                throw err;
            }
            throw new Error(`Extension failed, blocking execution: ${String(err)}`);
        }
    }
    /** `tool_result` handlers and image normalization. `parentToolCallId` is set for calls another tool made. */
    async _afterToolCall({ toolCall, args, result, isError }, parentToolCallId) {
        const runner = this._extensionRunner;
        const hookResult = runner.hasHandlers("tool_result")
            ? await runner.emitToolResult({
                type: "tool_result",
                toolName: toolCall.name,
                toolCallId: toolCall.id,
                ...(parentToolCallId ? { parentToolCallId } : {}),
                input: args,
                content: result.content,
                details: result.details,
                ...(result.structuredContent === undefined ? {} : { structuredContent: result.structuredContent }),
                isError,
                usage: result.usage,
            })
            : undefined;
        const content = hookResult?.content ?? result.content ?? [];
        // Runs after the extension hook so images injected or replaced by extensions are normalized too.
        const resizeOptions = this._limitsModel()?.inputLimits?.images?.resize;
        const normalizedContent = await normalizeToolResultImages(content, {
            autoResizeImages: this.settingsManager.getImageAutoResize(),
            ...(resizeOptions ? { resizeOptions } : {}),
        });
        if (!hookResult && normalizedContent === content) {
            return undefined;
        }
        // The hook result already dropped structured content that replaced content no longer matches.
        return {
            content: normalizedContent,
            details: hookResult?.details,
            structuredContent: hookResult ? hookResult.structuredContent : result.structuredContent,
            isError: hookResult?.isError ?? isError,
            usage: hookResult?.usage,
        };
    }
    /**
     * Run a call that the tool call `parentToolCallId` made through `ctx.executeTool()`. It goes
     * through the agent's tool pipeline with the session's hooks, against the callable tools.
     */
    async _executeNestedToolCall(parentToolCallId, name, args, options) {
        this._nestedToolCalls ??= new NestedToolCallRunner({
            getTools: () => this._getCallableTools(),
            isSequential: () => this.agent.toolExecution === "sequential",
            runToolCall: (toolCall, parentId, signal, onUpdate) => {
                const assistantMessage = this._findLastAssistantMessage();
                if (!assistantMessage) {
                    return Promise.resolve({
                        toolCall,
                        result: { content: [{ type: "text", text: "No assistant message issued this call" }], details: {} },
                        isError: true,
                    });
                }
                return runToolCall(toolCall, {
                    tools: this._getCallableTools(),
                    assistantMessage,
                    context: { messages: this.agent.state.messages, tools: this.agent.state.tools },
                    beforeToolCall: (context) => this._beforeToolCall(context, parentId),
                    afterToolCall: (context) => this._afterToolCall(context, parentId),
                    signal,
                    onUpdate,
                });
            },
            emit: async (event) => {
                await this._extensionRunner.emit(event);
                this._emit(event);
            },
        });
        return this._nestedToolCalls.execute(parentToolCallId, name, args, options);
    }
    /** Whether `projection`, the current session projection, exceeds the compaction threshold of `model`. */
    _exceedsCompactionThreshold(model, projection) {
        if (model.contextWindow <= 0)
            return false;
        return shouldCompact(estimateProjectedContextTokens(projection, this.sessionManager.getBranch()).tokens, model.contextWindow, this.settingsManager.getCompactionSettings(this.model));
    }
    async _compactBeforeNextAssistantResponse(context) {
        const projection = this.sessionManager.buildSessionProjection();
        // A virtual selection is checked in prepareRequest, against the model the request is routed to.
        const model = this.model;
        if (!model || isVirtualModel(model) || !this._exceedsCompactionThreshold(model, projection)) {
            return { ...context, messages: projection.messages };
        }
        await this._runAutoCompaction("threshold", false);
        return { ...context, messages: this.sessionManager.buildSessionProjection().messages };
    }
    _installAgentRequestProjection() {
        const previousPrepareRequest = this.agent.prepareRequest;
        this.agent.prepareRequest = async (request, signal) => {
            const failed = this._failedResponse;
            this._failedResponse = undefined;
            const prepare = async () => {
                const projection = this.sessionManager.buildSessionProjection();
                const canonicalContext = {
                    ...request.context,
                    messages: projection.messages,
                    // Messages declare the provider-visible loadout; context.tools keeps executable implementations.
                    tools: this.agent.state.tools.slice(),
                };
                const previous = await previousPrepareRequest?.({
                    ...request,
                    context: canonicalContext,
                    model: this.agent.state.model,
                    thinkingLevel: this.agent.state.thinkingLevel,
                }, signal);
                return { previous, context: previous?.context ?? canonicalContext, projection };
            };
            let { previous, context, projection } = await prepare();
            const model = previous?.model ?? this.agent.state.model;
            const thinkingLevel = previous?.thinkingLevel ?? this.agent.state.thinkingLevel;
            if (!isVirtualModel(model))
                return { ...previous, context, model, thinkingLevel };
            // The selection stays in agent state; only this request uses the routed model. A routing
            // failure rejects, which ends the run with an error response. Only messages the user wrote
            // start a turn; extension messages can follow them, e.g. from before_agent_start.
            const lastResponse = context.messages.findLastIndex((message) => message.role === "assistant");
            const userTurn = context.messages.slice(lastResponse + 1).some((message) => message.role === "user");
            const state = getVirtualModelState(this.sessionManager.getBranch(), model.provider, model.id);
            const route = await this._modelRuntime.resolveModel(model, convertToLlm(context.messages), {
                reason: failed ? "retry" : userTurn ? "user" : "continuation",
                thinkingLevel,
                signal,
                failed,
                state,
            });
            if (route.state !== undefined && route.state !== state) {
                const data = { provider: model.provider, modelId: model.id, state: route.state };
                const entry = this.sessionManager.getEntry(this.sessionManager.appendCustomEntry(VIRTUAL_MODEL_STATE_ENTRY, data));
                if (entry)
                    this._emit({ type: "entry_appended", entry });
            }
            // The route stands: the router already decided this request. The state entry does not change
            // the projection.
            if (this._exceedsCompactionThreshold(route.model, projection)) {
                await this._runAutoCompaction("threshold", false);
                ({ previous, context } = await prepare());
            }
            return { ...previous, context, model: route.model, thinkingLevel: route.thinkingLevel };
        };
    }
    async _dispatchTurnEndBoundary(message, toolResults) {
        this._lastActivityOutcome =
            message.stopReason === "aborted" ? "aborted" : message.stopReason === "error" ? "error" : "completed";
        const messageEntryId = this._findPersistedMessageEntryId(message);
        if (!this._extensionRunner.hasHandlers("turn_end"))
            return false;
        if (!messageEntryId) {
            this._extensionRunner.emitError({
                extensionPath: "<boundary>",
                event: "turn_end",
                error: "turn_end could not resolve the persisted assistant entry ID",
            });
            return false;
        }
        const toolResultEntryIds = toolResults.flatMap((result) => {
            const entryId = this._findPersistedMessageEntryId(result);
            return entryId ? [entryId] : [];
        });
        const boundary = await this._extensionRunner.emitBoundary({
            type: "turn_end",
            turnIndex: this._turnIndex,
            message,
            toolResults,
            messageEntryId,
            toolResultEntryIds,
            outcome: this._lastActivityOutcome,
        }, (entries) => this._buildBoundaryContext(entries, "turn_end"));
        this._commitBoundaryDrafts(boundary.entries);
        if (boundary.continue && !this._buildBoundaryContext([], "turn_end").canContinue) {
            this._reportInvalidBoundaryContinuation("turn_end");
            return false;
        }
        return boundary.continue;
    }
    _installAgentBoundaryHooks() {
        const previousFinishTurn = this.agent.finishTurn;
        this.agent.finishTurn = async (turn, signal) => {
            this._boundaryDispatchedMessages.add(turn.message);
            const extensionContinue = await this._dispatchTurnEndBoundary(turn.message, turn.toolResults);
            const previousDecision = await previousFinishTurn?.(turn, signal);
            if (previousDecision?.action === "end")
                return previousDecision;
            if (extensionContinue || previousDecision?.action === "continue")
                return { action: "continue" };
            return undefined;
        };
    }
    _installAgentNextTurnRefresh() {
        const previousPrepareNextTurnWithContext = this.agent.prepareNextTurnWithContext ??
            (this.agent.prepareNextTurn
                ? async (_turn, signal) => await this.agent.prepareNextTurn?.(signal)
                : undefined);
        this.agent.prepareNextTurnWithContext = async (turn, signal) => {
            const context = await this._compactBeforeNextAssistantResponse(turn.context);
            const previousSnapshot = await previousPrepareNextTurnWithContext?.({ ...turn, context }, signal);
            const nextContext = previousSnapshot?.context ?? context;
            const runOptions = this._runSystemPromptOptions ?? this._baseSystemPromptOptions;
            const options = normalizeBuildSystemPromptOptions({
                ...runOptions,
                selectedTools: this.getActiveToolNames(),
                toolSnippets: { ...this._baseSystemPromptOptions.toolSnippets, ...runOptions.toolSnippets },
                toolGuidelines: { ...this._baseSystemPromptOptions.toolGuidelines, ...runOptions.toolGuidelines },
            });
            const updateMessage = this._preparePromptAndToolLoadout(options, nextContext.messages);
            return {
                ...previousSnapshot,
                context: {
                    ...nextContext,
                    tools: this.agent.state.tools.slice(),
                },
                messages: updateMessage
                    ? [...(previousSnapshot?.messages ?? []), updateMessage]
                    : previousSnapshot?.messages,
                model: this.agent.state.model,
                thinkingLevel: this.agent.state.thinkingLevel,
            };
        };
    }
    // =========================================================================
    // Event Subscription
    // =========================================================================
    _refreshFinalizedContext() {
        const projection = this.sessionManager.buildSessionProjection();
        for (const entry of projection.entries) {
            for (const message of entry.messages)
                this._entryIdsByMessage.set(message, entry.sourceEntry.id);
        }
        this.agent.state.messages = projection.messages;
    }
    _applyBoundaryDrafts(manager, drafts) {
        const appended = [];
        for (const draft of drafts) {
            let entryId;
            switch (draft.type) {
                case "custom":
                    entryId = manager.appendCustomEntry(draft.customType, draft.data);
                    break;
                case "custom_message":
                    entryId = manager.appendCustomMessageEntry(draft.customType, draft.content, draft.display, draft.details);
                    break;
                case "context_edit":
                    entryId = manager.appendContextEdit(draft.targetId, draft.replacement);
                    break;
                case "compaction": {
                    const tokensBefore = estimateProjectedContextTokens(manager.buildSessionProjection(), manager.getBranch()).tokens;
                    entryId = manager.appendCompaction(draft.summary, draft.firstKeptEntryId, tokensBefore, draft.details, true, draft.usage);
                    break;
                }
            }
            const entry = manager.getEntry(entryId);
            if (entry)
                appended.push(entry);
        }
        return appended;
    }
    _createBoundaryPreviewManager(drafts) {
        const header = this.sessionManager.getHeader();
        if (!header)
            throw new Error("Session header is missing");
        const manager = SessionManager.inMemory(this._cwd, undefined, [header, ...this.sessionManager.getBranch()]);
        this._applyBoundaryDrafts(manager, drafts);
        return manager;
    }
    _getPendingBoundaryMessages() {
        return [...this.agent.peekQueuedMessages(), ...this._pendingCustomMessages];
    }
    _buildBoundaryContext(drafts, boundary) {
        const projection = this._createBoundaryPreviewManager(drafts).buildSessionProjection();
        const pendingMessages = this._getPendingBoundaryMessages();
        const llmMessages = convertToLlm(projection.messages);
        const finalRole = llmMessages[llmMessages.length - 1]?.role;
        const hasNonSystemContext = llmMessages.some((message) => message.role !== "system");
        const contextCanContinue = hasNonSystemContext && finalRole !== "assistant";
        const pendingCustomContext = this._pendingCustomMessages.length > 0;
        return {
            contextEntries: projection.entries,
            contextMessages: projection.messages,
            llmMessages,
            pendingMessages,
            canContinue: contextCanContinue ||
                pendingCustomContext ||
                (boundary === "turn_end"
                    ? this.agent.hasQueuedMessages()
                    : finalRole === "assistant" && this.agent.hasQueuedMessages()),
        };
    }
    _commitBoundaryDrafts(drafts) {
        const appended = this._applyBoundaryDrafts(this.sessionManager, drafts);
        this._refreshFinalizedContext();
        for (const entry of appended)
            this._emit({ type: "entry_appended", entry });
    }
    _reportInvalidBoundaryContinuation(event) {
        this._extensionRunner.emitError({
            extensionPath: "<boundary>",
            event,
            error: `${event} requested continuation without runnable model context`,
        });
    }
    /** Emit an event to all listeners */
    _emit(event) {
        for (const l of this._eventListeners) {
            l(event);
        }
    }
    _emitQueueUpdate() {
        this._emit({
            type: "queue_update",
            steering: [...this._steeringMessages],
            followUp: [...this._followUpMessages],
        });
    }
    async _emitSessionCompactFailed(event) {
        if (this._extensionRunner.hasHandlers("session_compact_failed")) {
            await this._extensionRunner.emit({ type: "session_compact_failed", ...event });
        }
    }
    _getIdleWaitPromise() {
        if (!this._idleWaitPromise) {
            this._idleWaitPromise = new Promise((resolve) => {
                this._resolveIdleWait = resolve;
            });
        }
        return this._idleWaitPromise;
    }
    _resolveIdleWaitIfIdle() {
        if (!this._isIdleAfterNativeCleanup() || !this._resolveIdleWait) {
            return;
        }
        const resolve = this._resolveIdleWait;
        this._idleWaitPromise = undefined;
        this._resolveIdleWait = undefined;
        resolve();
    }
    async _emitAgentSettled() {
        // The legacy idle getter becomes true before callbacks, but continuation admission
        // must remain busy through awaited observers and all nested native drain tails.
        this._settlementDepth++;
        try {
            this._cacheWarmer?.onAgentSettled();
            this._isAgentRunActive = false;
            this._isEmittingAgentSettled = true;
            try {
                await this._withNativeDeferralContext(async () => {
                    const aborted = this._agentRunAbortRequested;
                    await this._extensionRunner.emit({ type: "agent_settled", aborted });
                    this._emit({ type: "agent_settled", aborted });
                });
            }
            finally {
                this._isEmittingAgentSettled = false;
                if (!this._ownedDispatchActive)
                    await this._drainNativeActions();
            }
        }
        finally {
            this._settlementDepth--;
            this._resolveIdleWaitIfIdle();
            this._parentContinuation.notifyIdle();
        }
    }
    /** Internal handler for agent events - shared by subscribe and reconnect */
    _handleAgentEvent = async (event) => {
        // An abort requested while the run was starting, e.g. during before_agent_start, stops it
        // here. agent_start is awaited before the first request.
        if (event.type === "agent_start" && this._agentRunAbortRequested)
            this.agent.abort();
        // Record the calls a tool made through ctx.executeTool() and their usage on its result message.
        if (this._nestedToolCalls) {
            if (event.type === "message_start" && event.message.role === "toolResult") {
                const message = event.message;
                const summary = this._nestedToolCalls.takeRecord(message.toolCallId);
                if (summary?.calls)
                    message.nestedCalls = summary.calls;
                if (summary?.usage) {
                    message.usage = message.usage ? combineUsage(message.usage, summary.usage) : summary.usage;
                }
            }
            else if (event.type === "agent_end") {
                this._nestedToolCalls.clear();
            }
        }
        // When a user message starts, check if it's from either queue and remove it BEFORE emitting
        // This ensures the UI sees the updated queue state
        if (event.type === "message_start" && event.message.role === "user") {
            this._overflowRecoveryAttempted = false;
            const messageText = contentText(event.message.content, "");
            if (messageText) {
                // Check steering queue first
                const steeringIndex = this._steeringMessages.indexOf(messageText);
                if (steeringIndex !== -1) {
                    this._steeringMessages.splice(steeringIndex, 1);
                    this._emitQueueUpdate();
                }
                else {
                    // Check follow-up queue
                    const followUpIndex = this._followUpMessages.indexOf(messageText);
                    if (followUpIndex !== -1) {
                        this._followUpMessages.splice(followUpIndex, 1);
                        this._emitQueueUpdate();
                    }
                }
            }
        }
        // Emit to extensions first, then notify public listeners.
        await this._emitExtensionEvent(event);
        this._emit(event.type === "agent_end" ? { ...event, willRetry: this._willRetryAfterAgentEnd(event) } : event);
        // Handle session persistence
        if (event.type === "message_end") {
            let entryId;
            // Check if this is a custom message from extensions
            if (event.message.role === "custom") {
                // Persist as CustomMessageEntry
                entryId = this.sessionManager.appendCustomMessageEntry(event.message.customType, event.message.content, event.message.display, event.message.details);
            }
            else if (event.message.role === "system" ||
                event.message.role === "user" ||
                event.message.role === "assistant" ||
                event.message.role === "toolResult") {
                // Regular LLM message - persist as SessionMessageEntry
                entryId = this.sessionManager.appendMessage(event.message);
            }
            if (entryId)
                this._entryIdsByMessage.set(event.message, entryId);
            // Other message types (bashExecution, compactionSummary, branchSummary) are persisted elsewhere
            if (event.message.role === "assistant") {
                const assistantMsg = event.message;
                this._lastAssistantMessage = assistantMsg;
                if (assistantMsg.stopReason !== "error" && assistantMsg.stopReason !== "length") {
                    this._overflowRecoveryAttempted = false;
                }
                // Reset retry counter immediately on successful assistant response
                // This prevents accumulation across multiple LLM calls within a turn
                if (assistantMsg.stopReason !== "error" && this._retryAttempt > 0) {
                    this._emit({
                        type: "auto_retry_end",
                        success: true,
                        attempt: this._retryAttempt,
                    });
                    this._retryAttempt = 0;
                }
            }
        }
        // A turn ends after its assistant message and every tool result has been appended,
        // so this is the first point in the run where a context-only custom message can be
        // inserted without landing between a tool call and its result. Flushing after the
        // extension and listener dispatch above also picks up messages that turn_end
        // handlers queued.
        if (event.type === "turn_end") {
            this._flushPendingCustomMessages();
        }
    };
    _willRetryAfterAgentEnd(event) {
        if (this._agentRunAbortRequested)
            return false;
        const settings = this.settingsManager.getRetrySettings();
        if (!settings.enabled || this._retryAttempt >= settings.maxRetries) {
            return false;
        }
        for (let i = event.messages.length - 1; i >= 0; i--) {
            const message = event.messages[i];
            if (message.role === "assistant") {
                return this._isRetryableError(message);
            }
        }
        return false;
    }
    _findPersistedMessageEntryId(message) {
        const mapped = this._entryIdsByMessage.get(message);
        if (mapped)
            return mapped;
        for (const entry of [...this.sessionManager.getBranch()].reverse()) {
            if (entry.type === "message" && entry.message === message)
                return entry.id;
        }
        const messageIndex = this.agent.state.messages.indexOf(message);
        if (messageIndex < 0)
            return undefined;
        const projection = this.sessionManager.buildSessionProjection();
        let projectedIndex = 0;
        for (const entry of projection.entries) {
            for (let i = 0; i < entry.messages.length; i++) {
                if (projectedIndex === messageIndex) {
                    this._entryIdsByMessage.set(message, entry.sourceEntry.id);
                    return entry.sourceEntry.id;
                }
                projectedIndex++;
            }
        }
        return undefined;
    }
    /**
     * Persistently omit a failed attempt and its tool results from model context. Results are
     * found in the projected context, so the pair stays together whether the attempt comes from
     * the current run or a resumed session. An unpaired result fails the next request (#11227).
     */
    _omitRecoveryAttempt(message) {
        const toolCallIds = new Set(message.content.flatMap((block) => (block.type === "toolCall" ? [block.id] : [])));
        // A context edit replaces the projected copy, so match it by source entry as well.
        const entryId = this._findPersistedMessageEntryId(message);
        const messages = this.agent.state.messages;
        const index = messages.findIndex((m) => m === message || (entryId !== undefined && this._entryIdsByMessage.get(m) === entryId));
        const toolResults = [];
        for (const m of index < 0 ? [] : messages.slice(index + 1)) {
            if (m.role === "assistant")
                break;
            if (m.role === "toolResult" && toolCallIds.has(m.toolCallId))
                toolResults.push(m);
        }
        const targets = [message, ...toolResults];
        const targetIds = targets.map((target) => this._findPersistedMessageEntryId(target));
        const unresolvedProjectedTarget = targets.some((target, index) => targetIds[index] === undefined && this.agent.state.messages.includes(target));
        if (unresolvedProjectedTarget) {
            throw new Error("Cannot persist recovery omission because a projected message has no source entry");
        }
        for (const targetId of targetIds) {
            if (!targetId)
                continue;
            const editId = this.sessionManager.appendContextEdit(targetId, null);
            const entry = this.sessionManager.getEntry(editId);
            if (entry)
                this._emit({ type: "entry_appended", entry });
        }
        this._refreshFinalizedContext();
    }
    /** Find the last assistant message in agent state (including aborted ones) */
    _findLastAssistantMessage() {
        const messages = this.agent.state.messages;
        for (let i = messages.length - 1; i >= 0; i--) {
            const msg = messages[i];
            if (msg.role === "assistant") {
                return msg;
            }
        }
        return undefined;
    }
    _replaceMessageInPlace(target, replacement) {
        // Agent-core stores the finalized message object in its state before emitting message_end.
        // SessionManager persistence happens later in _handleAgentEvent() with event.message.
        // Mutating this object in place keeps agent state, later turn/agent events, listeners,
        // and the eventual SessionManager.appendMessage(event.message) persistence in sync.
        if (target === replacement) {
            return;
        }
        const targetRecord = target;
        for (const key of Object.keys(targetRecord)) {
            delete targetRecord[key];
        }
        Object.assign(targetRecord, replacement);
    }
    /** Emit extension events based on agent events */
    async _emitExtensionEvent(event) {
        if (event.type === "agent_start") {
            this._turnIndex = 0;
            await this._extensionRunner.emit({ type: "agent_start" });
        }
        else if (event.type === "agent_end") {
            await this._extensionRunner.emit({ type: "agent_end", messages: event.messages });
        }
        else if (event.type === "turn_start") {
            const extensionEvent = {
                type: "turn_start",
                turnIndex: this._turnIndex,
                timestamp: Date.now(),
            };
            await this._extensionRunner.emit(extensionEvent);
        }
        else if (event.type === "turn_end") {
            if (event.message.role === "assistant" && !this._boundaryDispatchedMessages.delete(event.message)) {
                await this._dispatchTurnEndBoundary(event.message, event.toolResults);
            }
            this._turnIndex++;
        }
        else if (event.type === "message_start") {
            const extensionEvent = {
                type: "message_start",
                message: event.message,
            };
            await this._extensionRunner.emit(extensionEvent);
        }
        else if (event.type === "message_update") {
            const extensionEvent = {
                type: "message_update",
                message: event.message,
                assistantMessageEvent: event.assistantMessageEvent,
            };
            await this._extensionRunner.emit(extensionEvent);
        }
        else if (event.type === "message_end") {
            const extensionEvent = {
                type: "message_end",
                message: event.message,
            };
            const replacement = await this._extensionRunner.emitMessageEnd(extensionEvent);
            if (replacement) {
                // Untyped extension handlers can return messages with null/missing content;
                // normalize so it never enters agent state or session history.
                const normalized = (replacement.role === "user" ||
                    replacement.role === "assistant" ||
                    replacement.role === "toolResult" ||
                    replacement.role === "custom") &&
                    replacement.content == null
                    ? { ...replacement, content: [] }
                    : replacement;
                this._replaceMessageInPlace(event.message, normalized);
            }
        }
        else if (event.type === "tool_execution_start") {
            const extensionEvent = {
                type: "tool_execution_start",
                toolCallId: event.toolCallId,
                toolName: event.toolName,
                args: event.args,
            };
            await this._extensionRunner.emit(extensionEvent);
        }
        else if (event.type === "tool_execution_update") {
            const extensionEvent = {
                type: "tool_execution_update",
                toolCallId: event.toolCallId,
                toolName: event.toolName,
                args: event.args,
                partialResult: event.partialResult,
            };
            await this._extensionRunner.emit(extensionEvent);
        }
        else if (event.type === "tool_execution_end") {
            const extensionEvent = {
                type: "tool_execution_end",
                toolCallId: event.toolCallId,
                toolName: event.toolName,
                result: event.result,
                isError: event.isError,
                ...(event.durationMs === undefined ? {} : { durationMs: event.durationMs }),
            };
            await this._extensionRunner.emit(extensionEvent);
        }
    }
    /**
     * Subscribe to agent events.
     * Session persistence is handled internally (saves messages on message_end).
     * Multiple listeners can be added. Returns unsubscribe function for this listener.
     */
    subscribe(listener) {
        this._eventListeners.push(listener);
        // Return unsubscribe function for this specific listener
        return () => {
            const index = this._eventListeners.indexOf(listener);
            if (index !== -1) {
                this._eventListeners.splice(index, 1);
            }
        };
    }
    /** Disconnect from agent events during disposal. */
    _disconnectFromAgent() {
        if (this._unsubscribeAgent) {
            this._unsubscribeAgent();
            this._unsubscribeAgent = undefined;
        }
    }
    /** Internal ownership transfer retires standalone facades before runtime callouts. */
    attachContinuationRuntimeGate(gate) {
        if (this._continuationRuntimeGate === gate)
            return;
        this._detachContinuationRuntimeGate?.();
        this._commandGeneration = {};
        this._continuationRuntimeGate = gate;
        this._detachContinuationRuntimeGate = gate.register(this._commandGateMember);
        this._parentContinuation.invalidateAll();
        if (this._parentContinuationDisposed) {
            this._detachContinuationRuntimeGate?.();
            this._detachContinuationRuntimeGate = undefined;
        }
    }
    hasContinuationRuntimeGate(gate) {
        return this._continuationRuntimeGate === gate;
    }
    /** Internal host lifecycle entry points; releases are synchronous and idempotent. */
    beginContinuationTransition() {
        return this._continuationGate.beginTransition();
    }
    beginContinuationInput() {
        return this._continuationGate.beginInput();
    }
    requestContinuationShutdown() {
        this._continuationGate.requestShutdown();
        this._detachContinuationRuntimeGate?.();
        this._detachContinuationRuntimeGate = undefined;
    }
    /**
     * Remove all listeners and disconnect from agent.
     * Call this when completely done with the session.
     */
    dispose() {
        this.requestContinuationShutdown();
        this._detachContinuationGate?.();
        this._detachContinuationGate = undefined;
        this._parentContinuationDisposed = true;
        this._parentContinuation.invalidateAll();
        try {
            this.abortRetry();
            this.abortCompaction();
            this.abortBranchSummary();
            this.abortBash();
            this.agent.abort();
        }
        catch {
            // Dispose must succeed even if an abort hook throws.
        }
        this._extensionRunner.invalidate("This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().");
        this._disconnectFromAgent();
        this._eventListeners = [];
        if (this._cacheWarmer) {
            this._cacheWarmer.onWarmed = undefined;
            this._cacheWarmer.cancel();
        }
        cleanupSessionResources(this.sessionId);
    }
    // =========================================================================
    // Read-only State Access
    // =========================================================================
    /** Refresh the public finalized transcript from the canonical session projection. */
    refreshContext() {
        this._refreshFinalizedContext();
    }
    /** Full agent state */
    get state() {
        return this.agent.state;
    }
    /** Current cache-warming state and the policy inputs that produced it. */
    get cacheWarmingStatus() {
        return this._cacheWarmer?.status;
    }
    /** Persist the cache-warming mode and immediately reconcile active warming. */
    setCacheWarmingMode(mode) {
        this.settingsManager.setCacheWarmingMode(mode);
        this._cacheWarmer?.onModeChanged();
    }
    /** Current model (may be undefined if not yet selected) */
    get model() {
        return this.agent.state.model;
    }
    /** Current thinking level */
    get thinkingLevel() {
        return this.agent.state.thinkingLevel;
    }
    /** Under a virtual selection, the physical model and thinking level of the latest successful response. */
    get routedModel() {
        if (!this.model || !isVirtualModel(this.model))
            return undefined;
        const latest = findLatestResponse(this.agent.state.messages);
        const model = latest && this._modelRuntime.getPhysicalModel(latest.provider, latest.model);
        return model && { model, thinkingLevel: latest?.thinkingLevel };
    }
    /** Whether the session is currently processing an agent run or post-run continuation. */
    get isStreaming() {
        return this._isAgentRunActive;
    }
    /** Whether the session has no active agent run, compaction, branch summary, retry, or queued continuation. */
    get isIdle() {
        return !this._isAgentRunActive && !this.isCompacting;
    }
    /** Current effective system prompt, including changes not yet sent to the model. */
    get systemPrompt() {
        return buildSystemPrompt(this._runSystemPromptOptions ?? this._baseSystemPromptOptions);
    }
    /** Current retry attempt (0 if not retrying) */
    get retryAttempt() {
        return this._retryAttempt;
    }
    /**
     * Get the names of currently active tools, which are the tools declared to the model.
     * Tools with `codemode` or `deferred` exposure are callable from other tools without being active.
     */
    getActiveToolNames() {
        return this.agent.state.tools.map((t) => t.name);
    }
    /** Get the names of the tools that tools can call through `ctx.executeTool()`. */
    getCallableToolNames() {
        return this._getCallableTools().map((t) => t.name);
    }
    /**
     * Get all configured tools with name, description, parameter schema, prompt guidelines, and source metadata.
     */
    getAllTools() {
        return Array.from(this._toolDefinitions.values()).map(({ definition, sourceInfo }) => ({
            name: definition.name,
            description: definition.description,
            parameters: definition.parameters,
            promptGuidelines: definition.promptGuidelines,
            exposure: this._getToolExposure(definition.name),
            ...(definition.namespace ? { namespace: definition.namespace } : {}),
            ...(definition.annotations ? { annotations: { ...definition.annotations } } : {}),
            ...(definition.reachesTools ? { reachesTools: definition.reachesTools } : {}),
            sourceInfo,
        }));
    }
    getToolDefinition(name) {
        return this._toolDefinitions.get(name)?.definition;
    }
    /**
     * Set active tools by name.
     * Only tools in the registry can be enabled. Unknown and hidden tool names are ignored.
     * Also rebuilds the system prompt to reflect the new tool set.
     * Changes take effect on the next agent turn.
     */
    setActiveToolsByName(toolNames) {
        this._setActiveTools(toolNames, true);
    }
    _committedToolRegistry() {
        return {
            registry: this._toolRegistry,
            definitions: this._toolDefinitions,
            snippets: this._toolPromptSnippets,
            guidelines: this._toolPromptGuidelines,
        };
    }
    _setActiveTools(toolNames, dropDeactivatedPending = false, context = this._committedToolRegistry(), revision = ++this._toolSelectionRevision) {
        const candidate = this._prepareToolSelection(toolNames, dropDeactivatedPending, context, revision);
        if (candidate)
            this._commitToolSelection(candidate, revision);
    }
    _prepareToolSelection(toolNames, dropDeactivatedPending, context, revision, strict = false) {
        const previous = this.getActiveToolNames();
        const loadout = this._prepareToolLoadout(toolNames, revision, context, strict);
        if (!loadout)
            return undefined;
        const names = loadout.tools.map((tool) => tool.name);
        const prompt = this._prepareSystemPrompt(names, loadout.hidden, revision, context);
        if (!prompt || revision !== this._toolSelectionRevision)
            return undefined;
        const runPrompt = this._runSystemPromptOptions
            ? normalizeBuildSystemPromptOptions({
                ...this._runSystemPromptOptions,
                toolSnippets: prompt.toolSnippets,
                toolGuidelines: prompt.toolGuidelines,
                selectedTools: names,
                hiddenTools: [...loadout.hidden],
            })
            : undefined;
        if (revision !== this._toolSelectionRevision)
            return undefined;
        const pending = new Set(this._pendingToolNames);
        for (const name of names)
            pending.delete(name);
        // Additive selections retain restored tools that have not registered yet.
        if (dropDeactivatedPending && previous.some((name) => !names.includes(name)))
            pending.clear();
        return { registry: context, tools: loadout.tools, hidden: loadout.hidden, prompt, runPrompt, pending };
    }
    _commitToolSelection(candidate, revision) {
        if (revision !== this._toolSelectionRevision)
            return false;
        // Callback-free publication: no resource access, metadata reads, or hook execution here.
        this._toolRegistry = candidate.registry.registry;
        this._toolDefinitions = candidate.registry.definitions;
        this._toolPromptSnippets = candidate.registry.snippets;
        this._toolPromptGuidelines = candidate.registry.guidelines;
        this.agent.state.tools = candidate.tools;
        this._hiddenDeclarations = candidate.hidden;
        this._baseSystemPromptOptions = candidate.prompt;
        this._runSystemPromptOptions = candidate.runPrompt;
        this._pendingToolNames = candidate.pending;
        return true;
    }
    /**
     * Whether `--tools` and `--exclude-tools` keep the tool registered. MCP tools stay registered
     * unless the allowlist filters them (see `_allowlistFiltersMcp`).
     */
    _isAllowedTool(name) {
        if (this._excludedTools?.(name))
            return false;
        if (!this._allowedTools || this._allowedTools(name))
            return true;
        return !this._allowlistFiltersMcp && isMcpToolName(name);
    }
    /**
     * Whether the tool may be active, which declares it to the model. MCP tools the allowlist keeps
     * without matching them are only for codemode and tool_search: they may be declared only when
     * tool_search can load them (non-`direct` exposure and tool_search registered). This also applies
     * to tools restored from the transcript or set by extensions.
     */
    _isActivatable(name, context = this._committedToolRegistry()) {
        if (!this._allowedTools || this._allowedTools(name) || !isMcpToolName(name))
            return true;
        return this._getToolExposure(name, context) !== "direct" && context.registry.has("tool_search");
    }
    _getToolExposure(name, context = this._committedToolRegistry()) {
        return context.definitions.get(name)?.definition.exposure ?? "direct";
    }
    /**
     * Tools callable through `ctx.executeTool()`: the active `direct` tools and every registered
     * `codemode` or `deferred` tool.
     */
    _getCallableTools(active = new Set(this.getActiveToolNames()), context = this._committedToolRegistry()) {
        return [...context.registry.values()].filter((tool) => {
            const exposure = this._getToolExposure(tool.name, context);
            return exposure === "codemode" || exposure === "deferred" || (exposure === "direct" && active.has(tool.name));
        });
    }
    /**
     * Prepare the agent's tools without publishing them. The active tools are
     * the registered, non-hidden ones; they are declared to the model. Active tools with a
     * `prepareLoadout` hook can change the declared descriptions and hide declarations from
     * requests (see {@link _installHiddenDeclarationsProjection}).
     *
     * The active tools are a set: tools that stay active keep their current order and newly active
     * ones are appended, so reordering the same names does not change the prompt (#11495).
     */
    _prepareToolLoadout(toolNames, revision, context = this._committedToolRegistry(), strict = false) {
        const requested = new Set(toolNames);
        const kept = this.getActiveToolNames().filter((name) => requested.has(name));
        const tools = [];
        for (const name of new Set([...kept, ...requested])) {
            const tool = context.registry.get(name);
            if (!tool)
                continue;
            const exposure = this._getToolExposure(name, context);
            if (revision !== this._toolSelectionRevision)
                return undefined;
            if (exposure === "hidden")
                continue;
            const activatable = this._isActivatable(name, context);
            if (revision !== this._toolSelectionRevision)
                return undefined;
            if (activatable)
                tools.push(tool);
        }
        const hooks = tools.flatMap((tool) => {
            const entry = context.definitions.get(tool.name);
            return entry?.definition.prepareLoadout ? [entry] : [];
        });
        const hidden = new Set();
        let declared = tools;
        if (hooks.length > 0) {
            const loadout = {
                declared: tools,
                callable: this._getCallableTools(new Set(tools.map((tool) => tool.name)), context),
                registered: [...context.registry.values()],
                getExposure: (name) => this._getToolExposure(name, context),
                getNamespace: (name) => context.definitions.get(name)?.definition.namespace,
                getPromptGuidelines: (name) => context.guidelines.get(name) ?? [],
            };
            const descriptions = new Map();
            for (const { definition, sourceInfo } of hooks) {
                if (revision !== this._toolSelectionRevision)
                    return undefined;
                const prepare = definition.prepareLoadout;
                if (revision !== this._toolSelectionRevision)
                    return undefined;
                // Legacy wrappers can change their delegate before one nested selection. Permit that
                // pass, but abort deeper recursion rather than skipping a required hook and committing.
                const depth = prepare ? (this._preparingToolHooks.get(prepare) ?? 0) : 0;
                if (depth >= 2)
                    return undefined;
                try {
                    if (prepare)
                        this._preparingToolHooks.set(prepare, depth + 1);
                    const changes = prepare?.call(definition, loadout);
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const changedDescriptions = changes?.descriptions ?? {};
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const descriptionNames = Object.keys(changedDescriptions);
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    for (const name of descriptionNames) {
                        const description = changedDescriptions[name];
                        if (revision !== this._toolSelectionRevision)
                            return undefined;
                        descriptions.set(name, description);
                    }
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const hiddenNames = changes?.hiddenDeclarations ?? [];
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const iterate = hiddenNames[Symbol.iterator];
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const iterator = iterate.call(hiddenNames);
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    const next = iterator.next;
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    // This is a value collection, not an owned resource. Do not run iterator.return
                    // on stale work: cleanup itself could supersede the winning selection.
                    while (true) {
                        const step = next.call(iterator);
                        if (revision !== this._toolSelectionRevision)
                            return undefined;
                        const done = step.done;
                        if (revision !== this._toolSelectionRevision)
                            return undefined;
                        if (done)
                            break;
                        const name = step.value;
                        if (revision !== this._toolSelectionRevision)
                            return undefined;
                        hidden.add(name);
                    }
                }
                catch (error) {
                    if (revision !== this._toolSelectionRevision)
                        return undefined;
                    if (strict)
                        throw error;
                    this._extensionRunner.emitError({
                        extensionPath: sourceInfo.path,
                        event: "prepare_loadout",
                        error: error instanceof Error ? error.message : String(error),
                        stack: error instanceof Error ? error.stack : undefined,
                    });
                }
                finally {
                    // Result accessors/iterators and error conversion/listeners can reenter too.
                    if (prepare) {
                        if (depth)
                            this._preparingToolHooks.set(prepare, depth);
                        else
                            this._preparingToolHooks.delete(prepare);
                    }
                }
            }
            declared = tools.map((tool) => {
                const description = descriptions.get(tool.name);
                return description === undefined ? tool : { ...tool, description };
            });
        }
        if (revision !== this._toolSelectionRevision)
            return undefined;
        return { tools: declared, hidden };
    }
    /** Whether compaction or branch summarization is currently running */
    get isCompacting() {
        return (this._autoCompactionAbortController !== undefined ||
            this._compactionAbortController !== undefined ||
            this._branchSummaryAbortController !== undefined);
    }
    /** All messages including custom types like BashExecutionMessage */
    get messages() {
        return this.agent.state.messages;
    }
    /** Current steering mode */
    get steeringMode() {
        return this.agent.steeringMode;
    }
    /** Current follow-up mode */
    get followUpMode() {
        return this.agent.followUpMode;
    }
    /** Current session file path, or undefined if sessions are disabled */
    get sessionFile() {
        return this.sessionManager.getSessionFile();
    }
    /** Current session ID */
    get sessionId() {
        return this.sessionManager.getSessionId();
    }
    /** Current session display name, if set */
    get sessionName() {
        return this.sessionManager.getSessionName();
    }
    /** Scoped models for cycling (from --models flag) */
    get scopedModels() {
        return this._scopedModels;
    }
    /** Update scoped models for cycling */
    setScopedModels(scopedModels) {
        this._scopedModels = scopedModels;
    }
    /** File-based prompt templates */
    get promptTemplates() {
        return this._resourceLoader.getPrompts().prompts;
    }
    _normalizePromptSnippet(text) {
        if (!text)
            return undefined;
        const oneLine = text
            .replace(/[\r\n]+/g, " ")
            .replace(/\s+/g, " ")
            .trim();
        return oneLine.length > 0 ? oneLine : undefined;
    }
    _normalizePromptGuidelines(guidelines) {
        if (!guidelines || guidelines.length === 0) {
            return [];
        }
        const unique = new Set();
        for (const guideline of guidelines) {
            const normalized = guideline.trim();
            if (normalized.length > 0) {
                unique.add(normalized);
            }
        }
        return Array.from(unique);
    }
    _rebuildSystemPrompt(toolNames) {
        const revision = this._toolSelectionRevision;
        const prompt = this._prepareSystemPrompt(toolNames, this._hiddenDeclarations, revision);
        if (prompt && revision === this._toolSelectionRevision)
            this._baseSystemPromptOptions = prompt;
    }
    _prepareSystemPrompt(toolNames, hidden, revision, context = this._committedToolRegistry()) {
        const validToolNames = toolNames.filter((name) => context.registry.has(name));
        const toolSnippets = {};
        for (const name of context.registry.keys()) {
            const snippet = context.snippets.get(name);
            // Tools without a snippet are not listed.
            if (snippet)
                toolSnippets[name] = snippet;
        }
        const loaderSystemPrompt = this._resourceLoader.getSystemPrompt();
        if (revision !== this._toolSelectionRevision)
            return undefined;
        const loaderAppendSystemPrompt = this._resourceLoader.getAppendSystemPrompt();
        if (revision !== this._toolSelectionRevision)
            return undefined;
        const appendSystemPrompt = loaderAppendSystemPrompt.length > 0 ? loaderAppendSystemPrompt.join("\n\n") : "";
        const loadedSkills = this._resourceLoader.getSkills().skills;
        if (revision !== this._toolSelectionRevision)
            return undefined;
        const loadedContextFiles = this._resourceLoader.getAgentsFiles().agentsFiles;
        if (revision !== this._toolSelectionRevision)
            return undefined;
        return normalizeBuildSystemPromptOptions({
            cwd: this._cwd,
            skills: loadedSkills,
            contextFiles: loadedContextFiles,
            customPrompt: loaderSystemPrompt,
            appendSystemPrompt,
            selectedTools: validToolNames,
            hiddenTools: [...hidden],
            toolSnippets,
            toolGuidelines: Object.fromEntries(context.guidelines),
        });
    }
    /**
     * Apply a prompt and tool loadout for the next request. Sets the executable tools and
     * returns a system message patching the prompt sections the model currently has (replayed
     * from `messages`), or undefined when the prompt is unchanged. Tool changes are declared by
     * the agent loop before the request.
     *
     * A forced prompt does not affect the transcript: the structured sections are still diffed
     * and persisted, and the forced text is projected onto the request by
     * {@link _installAgentForcedPromptProjection}.
     */
    _preparePromptAndToolLoadout(options, messages = this.agent.state.messages) {
        const revision = ++this._toolSelectionRevision;
        const loadout = this._prepareToolLoadout(options.selectedTools, revision);
        if (!loadout)
            return this._diffCommittedSystemPrompt(messages);
        const candidate = normalizeBuildSystemPromptOptions({
            ...options,
            selectedTools: loadout.tools.map((tool) => tool.name),
            hiddenTools: [...loadout.hidden],
        });
        if (revision !== this._toolSelectionRevision)
            return this._diffCommittedSystemPrompt(messages);
        const sections = diffSystemPromptSections(getCurrentSystemMessage(messages)?.sections ?? {}, buildSystemPromptSections(candidate));
        if (revision !== this._toolSelectionRevision)
            return this._diffCommittedSystemPrompt(messages);
        this.agent.state.tools = loadout.tools;
        this._hiddenDeclarations = loadout.hidden;
        // Callers must not publish their original options after a nested selection supersedes them.
        this._runSystemPromptOptions = candidate;
        return sections ? { role: "system", content: "", sections, timestamp: Date.now() } : undefined;
    }
    /** A stale candidate must not publish, but the request still needs the winning prompt sections. */
    _diffCommittedSystemPrompt(messages) {
        // Rendering can itself select tools through accessors. Read the new committed winner
        // once more, never rerunning loadout hooks or publishing abandoned run options.
        for (let attempt = 0; attempt < 2; attempt++) {
            const revision = this._toolSelectionRevision;
            const current = getCurrentSystemMessage(messages)?.sections ?? {};
            if (revision !== this._toolSelectionRevision)
                continue;
            const desired = buildSystemPromptSections(this._runSystemPromptOptions ?? this._baseSystemPromptOptions);
            if (revision !== this._toolSelectionRevision)
                continue;
            const sections = diffSystemPromptSections(current, desired);
            if (revision !== this._toolSelectionRevision)
                continue;
            return sections ? { role: "system", content: "", sections, timestamp: Date.now() } : undefined;
        }
        // Do not send a request with mismatched tools and prompt, or retry effectful rendering forever.
        throw new Error("Tool selection changed repeatedly while rendering the system prompt");
    }
    /**
     * Send a forced prompt as the provider's leading system prompt without recording it.
     *
     * A `before_agent_start` handler that returns `systemPrompt` needs that exact text at the
     * head of the request; a mid-conversation system message would leave the original prompt
     * in place. The forced text is a rendering of the current prompt, so the transcript keeps
     * its structured sections and the request is projected instead: the system messages
     * collapse into one head holding the forced text and the current tools. Runs after the
     * `context` extension handlers.
     */
    /**
     * Remove the declarations that `prepareLoadout` hooks hide from every request. The whole
     * transcript is filtered with the current set, so the projected declarations stay consistent
     * across requests and only change when the loadout does.
     */
    _installHiddenDeclarationsProjection() {
        const previousTransformContext = this.agent.transformContext;
        this.agent.transformContext = async (messages, signal) => {
            const transformed = previousTransformContext ? await previousTransformContext(messages, signal) : messages;
            const hidden = this._hiddenDeclarations;
            if (hidden.size === 0)
                return transformed;
            return transformed.map((message) => {
                if (message.role !== "system" || (!message.toolsAdded && !message.toolsRemoved))
                    return message;
                const { toolsAdded, toolsRemoved, ...rest } = message;
                const added = toolsAdded?.filter((tool) => !hidden.has(tool.name)) ?? [];
                const removed = toolsRemoved?.filter((tool) => !hidden.has(tool.name)) ?? [];
                return {
                    ...rest,
                    ...(added.length > 0 ? { toolsAdded: added } : {}),
                    ...(removed.length > 0 ? { toolsRemoved: removed } : {}),
                };
            });
        };
    }
    _installAgentForcedPromptProjection() {
        const previousTransformContext = this.agent.transformContext;
        this.agent.transformContext = async (messages, signal) => {
            const transformed = previousTransformContext ? await previousTransformContext(messages, signal) : messages;
            const forced = this._runSystemPromptOptions?.forceSystemPrompt;
            if (forced === undefined)
                return transformed;
            const current = getCurrentSystemMessage(transformed);
            const head = {
                role: "system",
                content: forced,
                ...(current?.toolsAdded ? { toolsAdded: current.toolsAdded } : {}),
                timestamp: current?.timestamp ?? Date.now(),
            };
            return [head, ...transformed.filter((message) => message.role !== "system")];
        };
    }
    /**
     * Restore the active tool loadout declared by the session transcript, if it declares one.
     * Tools reachable only from other tools are never declared, but they do not depend on the active
     * set, so the transcript's declarations are the whole loadout.
     * @param modifiers `+name`/`-name` entries applied to the restored loadout.
     */
    _restoreToolsFromTranscript(modifiers = []) {
        this._pendingToolNames.clear();
        // All system messages on the branch, including those before a compaction. Compaction snapshots
        // are derived from these, so replaying them gives the current loadout.
        const systemMessages = this.sessionManager
            .getBranch()
            .flatMap((entry) => (entry.type === "message" && entry.message.role === "system" ? [entry.message] : []));
        // No recorded loadout: keep the current tools.
        if (systemMessages.length === 0)
            return;
        // Tools the branch ever added or removed. Removed tools stay inactive.
        const mentioned = new Set(systemMessages
            .flatMap((message) => [...(message.toolsAdded ?? []), ...(message.toolsRemoved ?? [])])
            .map((tool) => tool.name));
        // Extension tools the branch never mentioned were registered after it recorded its loadout, so
        // they are activated like newly registered tools. Built-in tools follow `defaultTools` instead.
        const newTools = [...this._toolRegistry.keys()].filter((name) => !mentioned.has(name) && !this._baseToolDefinitions.has(name) && this._isActivatedOnRegistration(name));
        // The recorded loadout plus the new tools, with `--tools +name`/`-name` applied.
        const names = applyToolModifiers([...getCurrentTools(systemMessages).map((tool) => tool.name), ...newTools], modifiers);
        // Recorded tools that are not registered yet, such as MCP tools of servers still connecting, are
        // activated when they register. Excluded tools never register, so they are not kept pending.
        this._pendingToolNames = new Set(names.filter((name) => this._isAllowedTool(name)));
        // Ignores names that are not registered.
        this._setActiveTools(names);
    }
    // =========================================================================
    // Prompting
    // =========================================================================
    /** Native deferral and owned-command acceptance share lifetime, not authority.
     * Nested settlement inherits owned provenance; foreign drain actions explicitly clear it.
     * Expire the token so detached descendants cannot borrow acceptance from a later dispatch.
     */
    async _withNativeDeferralContext(action, owned) {
        const parent = AgentSession._nativeDeferralContext.getStore();
        const token = {
            owner: this,
            active: true,
            owned: owned ?? (parent?.active === true && parent.owner === this && parent.owned),
        };
        try {
            return await AgentSession._nativeDeferralContext.run(token, action);
        }
        finally {
            token.active = false;
        }
    }
    /** Hooks awaited by owned preparation cannot join its cleanup. External callers must join their own action. */
    async _withNativeInput(action, queued) {
        if (this._parentContinuationDisposed)
            throw new Error("AgentSession is disposed");
        this._parentContinuation.revokePending();
        const context = AgentSession._nativeDeferralContext.getStore();
        const descendant = context?.active && context.owner === this;
        if (this._isPreparingParentContinuation ||
            this._isEmittingAgentSettled ||
            (this._isDrainingNativeActions && !descendant)) {
            if (descendant) {
                this._deferredSettledActions.push(async () => {
                    try {
                        await this._withNativeInput(action, queued);
                    }
                    catch (error) {
                        // The hook already received acceptance, so its promise cannot carry this failure.
                        this._extensionRunner.emitError({
                            extensionPath: "<native-input>",
                            event: "deferred_native_input",
                            error: error instanceof Error ? error.message : String(error),
                        });
                        return { reportedError: error };
                    }
                });
                return queued;
            }
            return new Promise((resolve, reject) => {
                this._deferredSettledActions.push(async () => {
                    try {
                        resolve(await this._withNativeInput(action, queued));
                    }
                    catch (error) {
                        // This failure belongs to the submitting API/RPC call, not the owned receipt.
                        reject(error);
                    }
                });
            });
        }
        this._nativeInputDepth++;
        this._parentContinuation.notifyIdle();
        try {
            return await action();
        }
        finally {
            this._nativeInputDepth--;
            this._parentContinuation.notifyIdle();
        }
    }
    async _drainNativeActions(propagateFailure = true) {
        if (this._isPreparingParentContinuation || this._isDrainingNativeActions)
            return;
        this._isDrainingNativeActions = true;
        let failure;
        let unreportedFailure;
        try {
            while (this._deferredSettledActions.length) {
                const action = this._deferredSettledActions.shift();
                try {
                    const token = { owner: this, active: true };
                    let result;
                    try {
                        result = await AgentSession._nativeDrainContext.run(token, () => this._withNativeDeferralContext(action, false));
                    }
                    finally {
                        token.active = false;
                    }
                    if (result)
                        failure ??= { error: result.reportedError };
                }
                catch (error) {
                    // A rejected preflight must not strand later accepted native actions.
                    failure ??= { error };
                    unreportedFailure ??= { error };
                }
            }
            // Deferred wrappers have already rejected the external caller or reported the descendant error.
            // Only native-only settlement retains the legacy joined failure semantics.
            if (failure && propagateFailure)
                throw failure.error;
            if (unreportedFailure)
                throw unreportedFailure.error;
        }
        finally {
            this._isDrainingNativeActions = false;
            this._resolveIdleWaitIfIdle();
            this._parentContinuation.notifyIdle();
        }
    }
    async _startRun(trigger, onStarted, owned) {
        if (!owned)
            return this._startRunPrepared(trigger, onStarted);
        this._ownedDispatchActive = true;
        this._isPreparingParentContinuation = true;
        try {
            await this._withNativeDeferralContext(() => this._startRunPrepared(trigger, onStarted, owned), true);
        }
        finally {
            this._isPreparingParentContinuation = false;
        }
    }
    /**
     * Prepare and start a run. Ordinary runs count as streaming during preparation;
     * owned preparation instead defers runnable native input until cleanup. An abort
     * requested meanwhile aborts the run as soon as it exists.
     * @param onStarted Called once the run's messages are prepared, before the first request.
     */
    async _startRunPrepared(trigger, onStarted, owned) {
        this._isAgentRunActive = true;
        this._agentRunAbortRequested = false;
        let messages;
        // The hook trigger is extension-owned once preparation begins. Never read its payload
        // back for history or public notice events; the frozen request owns the bounded text.
        const ownedNotice = owned
            ? {
                role: "custom",
                customType: "pi-parent-continuation",
                content: owned.request.content,
                display: true,
                timestamp: Date.now(),
            }
            : undefined;
        const stagedNextTurn = owned ? [] : undefined;
        try {
            messages = await this._prepareRun(trigger, stagedNextTurn);
        }
        catch (error) {
            this._isAgentRunActive = false;
            this._flushPendingBashMessages();
            this._flushPendingCustomMessages();
            this._resolveIdleWaitIfIdle();
            throw error;
        }
        // Messages held while the run was starting go before its starting messages, as they would
        // have while idle. No run exists yet, so they cannot split a tool call from its result.
        this._flushPendingBashMessages();
        this._flushPendingCustomMessages();
        onStarted?.();
        // Compaction before the prompt may have scheduled a retry; the new prompt replaces it.
        this._failedResponse = undefined;
        this._recordSelection();
        // The run records the loadout in the transcript; restored tools that did not register by now
        // are dropped, so a tool that never registers does not stay pending.
        this._pendingToolNames.clear();
        try {
            if (owned && ownedNotice && trigger.messageType === "custom") {
                // Exclude only the hook trigger identity, not other messages with matching text.
                messages = messages.filter((message) => message !== trigger.message);
                if (!owned.control.canCommit())
                    return;
                try {
                    this.sessionManager.appendCustomMessageEntry(ownedNotice.customType, ownedNotice.content, ownedNotice.display, undefined);
                    // Projection can read public historical objects with accessors. If they
                    // revoke authority, the append still exists and cannot be reported as none.
                    this._refreshFinalizedContext();
                    if (!owned.control.markCommitted())
                        throw new Error("Owned notice invalidated during historical commit");
                }
                catch (error) {
                    // Persistence may already have mutated history; never classify this as a clean cancellation.
                    owned.control.markCommitUnknown();
                    throw error;
                }
                this._emit({ type: "message_start", message: { ...ownedNotice } });
                this._emit({ type: "message_end", message: { ...ownedNotice } });
                if (!owned.control.markStarted())
                    return;
                this._parentContinuationToken = owned.request.token;
                this._pendingNextTurnMessages = this._pendingNextTurnMessages.filter((message) => !stagedNextTurn.includes(message));
            }
            this._isPreparingParentContinuation = false;
            await this.agent.prompt(messages);
            while (!this._agentRunAbortRequested) {
                if (await this._handlePostAgentRun()) {
                    if (this._agentRunAbortRequested)
                        break;
                    await this.agent.continue();
                    continue;
                }
                if (this._agentRunAbortRequested || !(await this._runBeforeSettleBoundary()))
                    break;
                if (this._agentRunAbortRequested)
                    break;
                await this.agent.continue();
            }
        }
        finally {
            if (owned && this._parentContinuationToken === owned.request.token)
                this._parentContinuationToken = undefined;
            if (this._agentRunAbortRequested)
                this._finishCancelledRetry();
            this._failedResponse = undefined;
            this._runSystemPromptOptions = undefined;
            this._flushPendingBashMessages();
            this._flushPendingCustomMessages();
            await this._emitAgentSettled();
        }
    }
    /**
     * Prepare a run and return its starting messages: compact first when needed, emit
     * before_agent_start, apply the run's prompt options and tool loadout, and build the starting
     * message followed by pending nextTurn messages and handler messages, with a system patch first
     * when the prompt changed. Owned runs stage nextTurn references for consumption at admission;
     * ordinary runs consume them here.
     */
    async _prepareRun(trigger, stagedNextTurn) {
        // Check if we need to compact before sending (catches aborted responses).
        // The starting message is sent below, so do not call agent.continue() here.
        const lastAssistant = this._findLastAssistantMessage();
        if (lastAssistant) {
            await this._checkCompaction(lastAssistant, false);
        }
        const selectedToolsBefore = this._baseSystemPromptOptions.selectedTools;
        const result = await this._extensionRunner.emitBeforeAgentStart(trigger, this._baseSystemPromptOptions);
        // Handlers may edit event.systemPromptOptions.selectedTools or call setActiveTools(),
        // which updates the live loadout instead. An explicit edit wins; otherwise the live
        // loadout is authoritative, so a setActiveTools() call is not undone here.
        const handlerEditedTools = result.systemPromptOptions.selectedTools.length !== selectedToolsBefore.length ||
            result.systemPromptOptions.selectedTools.some((name, index) => name !== selectedToolsBefore[index]);
        if (!handlerEditedTools)
            result.systemPromptOptions.selectedTools = this.getActiveToolNames();
        const messages = [];
        if (trigger.messageType === "custom") {
            messages.push(trigger.message);
        }
        else {
            // Normalize images after before_agent_start so extension-driven model selection
            // determines the resize profile used for the request and history.
            const normalized = await this._normalizePromptImages(trigger.images);
            const text = normalized.hints.length > 0 ? `${trigger.prompt}\n\n${normalized.hints.join("\n")}` : trigger.prompt;
            messages.push({
                role: "user",
                content: [{ type: "text", text }, ...normalized.images],
                ...(typeof trigger.displayText === "string" ? { displayText: trigger.displayText } : {}),
                timestamp: Date.now(),
            });
        }
        // Inject any pending "nextTurn" messages as context alongside the starting message
        messages.push(...this._pendingNextTurnMessages);
        // Owned preparation cannot consume another extension's messages before native admission.
        if (stagedNextTurn)
            stagedNextTurn.push(...this._pendingNextTurnMessages);
        else
            this._pendingNextTurnMessages = [];
        for (const msg of result.messages) {
            messages.push({
                role: "custom",
                customType: msg.customType,
                // Untyped extensions can pass null/missing content; normalize at ingestion.
                content: msg.content ?? [],
                display: msg.display,
                details: msg.details,
                timestamp: Date.now(),
            });
        }
        const updateMessage = this._preparePromptAndToolLoadout(result.systemPromptOptions);
        if (updateMessage)
            messages.unshift(updateMessage);
        return messages;
    }
    async _handlePostAgentRun() {
        const message = this._lastAssistantMessage;
        this._lastAssistantMessage = undefined;
        if (this._agentRunAbortRequested) {
            this._finishCancelledRetry();
            return false;
        }
        if (!message)
            return this.agent.hasQueuedMessages();
        if (this._isRetryableError(message) && (await this._prepareRetry(message))) {
            if (this._agentRunAbortRequested)
                this._finishCancelledRetry();
            this._failedResponse = message;
            return !this._agentRunAbortRequested;
        }
        if (this._agentRunAbortRequested) {
            this._finishCancelledRetry();
            return false;
        }
        if (message.stopReason === "error" && this._retryAttempt > 0) {
            this._emit({
                type: "auto_retry_end",
                success: false,
                attempt: this._retryAttempt,
                finalError: message.errorMessage,
            });
            this._retryAttempt = 0;
        }
        if (await this._checkCompaction(message)) {
            return !this._agentRunAbortRequested;
        }
        // The low-level loop drains both queues before agent_end. Messages queued by
        // agent_end handlers require a fresh run before pre-settlement handlers fire.
        return !this._agentRunAbortRequested && this.agent.hasQueuedMessages();
    }
    async _runBeforeSettleBoundary() {
        if (!this._extensionRunner.hasHandlers("agent_before_settle"))
            return this.agent.hasQueuedMessages();
        this._isBeforeSettle = true;
        this._abortDuringBeforeSettle = false;
        try {
            const result = await this._extensionRunner.emitBoundary({ type: "agent_before_settle", outcome: this._lastActivityOutcome }, (entries) => this._buildBoundaryContext(entries, "agent_before_settle"));
            this._commitBoundaryDrafts(result.entries);
            this._flushPendingCustomMessages();
            const finalContext = this._buildBoundaryContext([], "agent_before_settle");
            if (this._abortDuringBeforeSettle)
                return false;
            const shouldContinue = result.continue || this.agent.hasQueuedMessages();
            if (shouldContinue && !finalContext.canContinue) {
                if (result.continue)
                    this._reportInvalidBoundaryContinuation("agent_before_settle");
                return false;
            }
            return shouldContinue;
        }
        finally {
            this._isBeforeSettle = false;
        }
    }
    async _runInputHandlers(text, images, source, streamingBehavior) {
        if (!this._extensionRunner.hasHandlers("input")) {
            return { text, images };
        }
        const inputResult = await this._extensionRunner.emitInput(text, images, source, streamingBehavior);
        if (inputResult.action === "handled") {
            return undefined;
        }
        if (inputResult.action === "transform") {
            return { text: inputResult.text, images: inputResult.images ?? images };
        }
        return { text, images };
    }
    async _normalizePromptImages(images) {
        if (!images)
            return { images: [], hints: [] };
        const normalizedImages = [];
        const hints = [];
        for (const image of images) {
            const processed = await processImage(Buffer.from(image.data, "base64"), image.mimeType, {
                autoResizeImages: this.settingsManager.getImageAutoResize(),
                resizeOptions: this._limitsModel()?.inputLimits?.images?.resize,
            });
            if (!processed.ok) {
                hints.push(processed.message);
                continue;
            }
            normalizedImages.push({ type: "image", data: processed.data, mimeType: processed.mimeType });
            hints.push(...processed.hints);
        }
        return { images: normalizedImages, hints };
    }
    /**
     * Send a prompt to the agent.
     * - Handles extension commands (registered via pi.registerCommand) immediately, even during streaming
     * - Expands file-based prompt templates by default
     * - During streaming, queues via steer() or followUp() based on streamingBehavior option
     * - Validates model and API key before sending (when not streaming)
     * @throws Error if streaming and no streamingBehavior specified
     * @throws Error if no model selected or no API key available (when not streaming)
     */
    async prompt(text, options) {
        if (this._parentContinuationDisposed)
            throw new Error("AgentSession is disposed");
        const release = this._parentContinuation.beginControl();
        try {
            const invocation = this.prepareExtensionCommand(text);
            try {
                const expand = options?.expandPromptTemplates ?? true;
                if (expand && invocation) {
                    const preflightResult = options?.preflightResult;
                    const context = AgentSession._nativeDeferralContext.getStore();
                    const completion = invocation.run();
                    // Only an owned dispatch descendant receives acceptance instead of joining
                    // a lifecycle tail that needs its lease closed. Native-only settlement and
                    // queued external input retain actual command completion.
                    if (context?.active && context.owner === this && context.owned) {
                        void completion.catch((error) => {
                            // Handler errors were already reported. A throwing reporter has no
                            // owned result channel; retain its failure for an external idle join.
                            this._commandTailFailure ??= { error };
                        });
                    }
                    else
                        await completion;
                    preflightResult?.("handled");
                    return;
                }
                return this._withNativeInput(() => this._prompt(text, options, expand), undefined);
            }
            finally {
                invocation?.dispose();
            }
        }
        finally {
            release();
        }
    }
    /** Host input already classified as runnable; never reinterpret it as a command. */
    async promptInput(text, options) {
        if (this._parentContinuationDisposed)
            throw new Error("AgentSession is disposed");
        return this._withNativeInput(() => this._prompt(text, options, options?.expandPromptTemplates ?? true), undefined);
    }
    async _prompt(text, options, expandPromptTemplates) {
        const preflightResult = options?.preflightResult;
        if (this._compactionAbortController !== undefined) {
            throw new Error("Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.");
        }
        // Emit input event for extension interception (before skill/template expansion)
        const processedInput = await this._runInputHandlers(text, options?.images, options?.source ?? "interactive", this.isStreaming ? options?.streamingBehavior : undefined);
        if (!processedInput) {
            preflightResult?.("handled");
            return;
        }
        const { text: currentText, images: currentImages } = processedInput;
        // Expand skill commands (/skill:name args) and prompt templates (/template args)
        let expandedText = currentText;
        let displayText;
        if (expandPromptTemplates) {
            expandedText = this._expandSkillCommand(expandedText);
            const beforeTemplateExpansion = expandedText;
            expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);
            // Retain the submitted invocation, even when input handlers transform it.
            if (expandedText !== beforeTemplateExpansion) displayText = text;
        }
        // If streaming, queue via steer() or followUp() based on option
        if (this.isStreaming) {
            if (!options?.streamingBehavior) {
                throw new Error("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.");
            }
            if (options.streamingBehavior === "followUp") {
                await this._queueFollowUp(expandedText, currentImages, displayText);
            }
            else {
                await this._queueSteer(expandedText, currentImages, displayText);
            }
            preflightResult?.("queued");
            return;
        }
        // Validate model
        if (!this.model) {
            throw new Error(formatNoModelSelectedMessage());
        }
        const hasConfiguredAuth = this._modelRuntime.hasConfiguredAuth(this.model.provider) ||
            (await this._modelRuntime.checkAuth(this.model.provider)) !== undefined;
        if (!hasConfiguredAuth) {
            const isOAuth = this._modelRuntime.isUsingOAuth(this.model.provider);
            if (isOAuth) {
                throw new Error(`Authentication failed for "${this.model.provider}". ` +
                    `Credentials may have expired or network is unavailable. ` +
                    `Run '/login ${this.model.provider}' to re-authenticate.`);
            }
            throw new Error(formatNoApiKeyFoundMessage(this.model.provider));
        }
        await this._startRun({ messageType: "user", prompt: expandedText, images: currentImages, displayText }, () => preflightResult?.("started"));
    }
    /**
     * Internal epoch fence, captured before editor/descriptor callouts. This acquires no
     * admission ticket: hosts must hold command control separately until input transfers.
     */
    captureCommandInputGuard() {
        const runner = this._extensionRunner;
        const generation = this._commandGeneration;
        const runnerCurrent = runner.captureCommandGuard();
        return () => generation === this._commandGeneration &&
            runner === this._extensionRunner &&
            !this._parentContinuationDisposed &&
            this._continuationGate.isAuthorityAvailable() &&
            (this._continuationRuntimeGate?.isAuthorityAvailable() ?? true) &&
            runnerCurrent();
    }
    /** Internal host admission fence: acquire before reading editor/options; always release in finally. */
    beginExtensionCommandControl() {
        return this._parentContinuation.beginControl();
    }
    /**
     * Internal host capture, not an extension protocol. The handle owns a control ticket:
     * run releases it on completion; dispose abandons an unrun handle. Both are one-shot.
     * Hosts must hold an outer control fence before editor callouts and dispose in finally.
     * A miss releases its ticket synchronously; the host must enter runnable input before yielding.
     * Capture invalidation throws rather than returning a miss. Already captured stale handles
     * are consumed without execution; "handled" means consumed input, not
     * proof that a handler ran. Direct prompt rejects an already disposed session.
     */
    prepareExtensionCommand(text) {
        const release = this._parentContinuation.beginControl();
        let transferred = false;
        try {
            if (!text.startsWith("/"))
                return undefined;
            const spaceIndex = text.indexOf(" ");
            const commandName = spaceIndex === -1 ? text.slice(1) : text.slice(1, spaceIndex);
            const args = spaceIndex === -1 ? "" : text.slice(spaceIndex + 1);
            const runner = this._extensionRunner;
            const current = this.captureCommandInputGuard();
            const command = runner.getCommand(commandName);
            if (!current())
                throw new Error("Command input capture invalidated");
            if (!command)
                return undefined;
            const handler = command.handler;
            if (!current())
                throw new Error("Command input capture invalidated");
            let consumed = false;
            transferred = true;
            return Object.freeze({
                dispose: () => {
                    if (consumed)
                        return;
                    consumed = true;
                    release();
                },
                run: () => {
                    if (consumed)
                        return Promise.resolve();
                    consumed = true;
                    const token = { owner: this, active: true };
                    let resolve;
                    let reject;
                    const completion = new Promise((res, rej) => {
                        resolve = res;
                        reject = rej;
                    });
                    // Register before any handler callout: its synchronous prefix can start shutdown.
                    this._commandTails.add(completion);
                    const execute = async () => {
                        try {
                            if (!current())
                                return;
                            const ctx = runner.createCommandContext();
                            if (!current())
                                return;
                            try {
                                await handler(args, ctx);
                            }
                            catch (err) {
                                // Reload replaces the runner and moves bound error listeners with it.
                                this._extensionRunner.emitError({
                                    extensionPath: `command:${commandName}`,
                                    event: "command",
                                    error: err instanceof Error ? err.message : String(err),
                                });
                            }
                        }
                        finally {
                            token.active = false;
                            this._commandTails.delete(completion);
                            release();
                        }
                    };
                    void AgentSession._commandTailContext.run(token, execute).then(resolve, reject);
                    return completion;
                },
            });
        }
        finally {
            if (!transferred)
                release();
        }
    }
    /**
     * Expand skill commands (/skill:name args) to their full content.
     * Returns the expanded text, or the original text if not a skill command or skill not found.
     * Emits errors via extension runner if file read fails.
     */
    _expandSkillCommand(text) {
        if (!text.startsWith("/skill:"))
            return text;
        const spaceIndex = text.indexOf(" ");
        const skillName = spaceIndex === -1 ? text.slice(7) : text.slice(7, spaceIndex);
        const args = spaceIndex === -1 ? "" : text.slice(spaceIndex + 1).trim();
        const skill = this.resourceLoader.getSkills().skills.find((s) => s.name === skillName);
        if (!skill)
            return text; // Unknown skill, pass through
        try {
            const content = readFileSync(skill.filePath, "utf-8");
            const body = stripFrontmatter(content).trim();
            const skillBlock = `<skill name="${skill.name}" location="${skill.filePath}">\nReferences are relative to ${skill.baseDir}.\n\n${body}\n</skill>`;
            return args ? `${skillBlock}\n\n${args}` : skillBlock;
        }
        catch (err) {
            // Emit error like extension commands do
            this._extensionRunner.emitError({
                extensionPath: skill.filePath,
                event: "skill_expansion",
                error: err instanceof Error ? err.message : String(err),
            });
            return text; // Return original on error
        }
    }
    async _queueUserInput(text, images, behavior, source) {
        if (text.startsWith("/")) {
            this._throwIfExtensionCommand(text);
        }
        return this._queueNativeInput(text, images, behavior, source);
    }
    /** Queue host-classified native input without command lookup or starting a turn. */
    async queueInput(text, behavior, images, options) {
        return this._withNativeInput(() => this._queueNativeInput(text, images, behavior, options?.source ?? "interactive"), "queued");
    }
    async _queueNativeInput(text, images, behavior, source) {
        const processedInput = await this._runInputHandlers(text, images, source, this.isStreaming ? behavior : undefined);
        if (!processedInput)
            return "handled";
        let expandedText = this._expandSkillCommand(processedInput.text);
        const beforeTemplateExpansion = expandedText;
        expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);
        const displayText = expandedText !== beforeTemplateExpansion ? text : undefined;
        if (behavior === "steer") {
            await this._queueSteer(expandedText, processedInput.images, displayText);
        }
        else {
            await this._queueFollowUp(expandedText, processedInput.images, displayText);
        }
        return "queued";
    }
    /**
     * Queue a steering message while the agent is running.
     * Delivered after the current assistant turn finishes executing its tool calls,
     * before the next LLM call.
     * Expands skill commands and prompt templates. Errors on extension commands.
     * @param images Optional image attachments to include with the message
     * @param options Input source; defaults to interactive
     * @throws Error if text is an extension command
     */
    async steer(text, images, options) {
        return this._withNativeInput(() => this._queueUserInput(text, images, "steer", options?.source ?? "interactive"), "queued");
    }
    /**
     * Queue a follow-up message to be processed after the agent finishes.
     * Delivered only when agent has no more tool calls or steering messages.
     * Expands skill commands and prompt templates. Errors on extension commands.
     * @param images Optional image attachments to include with the message
     * @param options Input source; defaults to interactive
     * @throws Error if text is an extension command
     */
    async followUp(text, images, options) {
        return this._withNativeInput(() => this._queueUserInput(text, images, "followUp", options?.source ?? "interactive"), "queued");
    }
    /**
     * Internal: Queue a steering message (already expanded, no extension command check).
     */
    async _queueSteer(text, images, displayText) {
        this._steeringMessages.push(text);
        this._emitQueueUpdate();
        const content = [{ type: "text", text }];
        if (images) {
            content.push(...images);
        }
        this.agent.steer({
            role: "user",
            content,
            ...(typeof displayText === "string" ? { displayText } : {}),
            timestamp: Date.now(),
        });
    }
    /**
     * Internal: Queue a follow-up message (already expanded, no extension command check).
     */
    async _queueFollowUp(text, images, displayText) {
        this._followUpMessages.push(text);
        this._emitQueueUpdate();
        const content = [{ type: "text", text }];
        if (images) {
            content.push(...images);
        }
        this.agent.followUp({ role: "user", content, ...(typeof displayText === "string" ? { displayText } : {}), timestamp: Date.now() });
    }
    /**
     * Throw an error if the text is an extension command.
     */
    _throwIfExtensionCommand(text) {
        const spaceIndex = text.indexOf(" ");
        const commandName = spaceIndex === -1 ? text.slice(1) : text.slice(1, spaceIndex);
        const command = this._extensionRunner.getCommand(commandName);
        if (command) {
            throw new Error(`Extension command "/${commandName}" cannot be queued. Use prompt() or execute the command when not streaming.`);
        }
    }
    /**
     * Send a custom message to the session. Creates a CustomMessageEntry.
     *
     * Handles four cases:
     * - Streaming: queues message, processed when loop pulls from queue
     * - Streaming + triggerTurn false: appended to state/session once the current turn ends
     * - Not streaming + triggerTurn: appends to state/session, starts new turn
     * - Not streaming + no trigger: appends to state/session, no turn
     *
     * @param message Custom message with customType, content, display, details
     * @param options.triggerTurn If true and not streaming, triggers a new LLM turn
     * @param options.deliverAs Delivery mode: "steer", "followUp", or "nextTurn"
     */
    async sendCustomMessage(message, options) {
        const runnable = options?.deliverAs !== "nextTurn" &&
            (this.isStreaming ? options?.triggerTurn !== false : options?.triggerTurn === true);
        if (runnable)
            return this._withNativeInput(() => this._sendCustomMessage(message, { ...options, triggerTurn: true }), undefined);
        return this._sendCustomMessage(message, options);
    }
    async _sendCustomMessage(message, options) {
        const appMessage = {
            role: "custom",
            customType: message.customType,
            // Untyped extensions can pass null/missing content; normalize at ingestion.
            content: message.content ?? [],
            display: message.display,
            details: message.details,
            timestamp: Date.now(),
        };
        if (options?.deliverAs === "nextTurn") {
            this._pendingNextTurnMessages.push(appMessage);
        }
        else if (this.isStreaming && options?.triggerTurn !== false) {
            if (options?.deliverAs === "followUp") {
                this.agent.followUp(appMessage);
            }
            else {
                this.agent.steer(appMessage);
            }
        }
        else if (options?.triggerTurn) {
            await this._startRun({ messageType: "custom", message: appMessage });
        }
        else if (this.isStreaming) {
            // Appending now would put the message between an assistant tool call and its
            // result, which providers that validate message order reject on replay. Defer
            // to the end of the turn. Nothing is emitted yet: message events must not
            // describe messages the session tree does not contain.
            this._pendingCustomMessages.push(appMessage);
        }
        else {
            this._appendCustomMessage(appMessage);
        }
    }
    _appendCustomMessage(appMessage) {
        this.sessionManager.appendCustomMessageEntry(appMessage.customType, appMessage.content, appMessage.display, appMessage.details);
        this._refreshFinalizedContext();
        this._emit({ type: "message_start", message: appMessage });
        this._emit({ type: "message_end", message: appMessage });
    }
    /**
     * Append custom messages queued while the agent was running.
     * Called once the current turn's tool results are in agent state and session history.
     */
    _flushPendingCustomMessages() {
        if (this._pendingCustomMessages.length === 0)
            return;
        const pending = this._pendingCustomMessages;
        this._pendingCustomMessages = [];
        for (const appMessage of pending) {
            this._appendCustomMessage(appMessage);
        }
    }
    /**
     * Send a user message to the agent. Always triggers a turn.
     * When the agent is streaming, use deliverAs to specify how to queue the message.
     *
     * @param content User message content (string or content array)
     * @param options.deliverAs Delivery mode when streaming: "steer" or "followUp"
     * @param options.expandPromptTemplates Whether to dispatch extension commands and expand skill commands and prompt templates. Default: false.
     */
    async sendUserMessage(content, options) {
        return this._withNativeInput(() => this._sendUserMessage(content, options), undefined);
    }
    async _sendUserMessage(content, options) {
        // Normalize content to text string + optional images
        let text;
        let images;
        if (typeof content === "string") {
            text = content;
        }
        else {
            const textParts = [];
            images = [];
            for (const part of content) {
                if (part.type === "text") {
                    textParts.push(part.text);
                }
                else {
                    images.push(part);
                }
            }
            text = textParts.join("\n");
            if (images.length === 0)
                images = undefined;
        }
        await this.prompt(text, {
            expandPromptTemplates: options?.expandPromptTemplates ?? false,
            streamingBehavior: options?.deliverAs,
            images,
            source: "extension",
        });
    }
    /**
     * Clear all queued messages and return them.
     * Useful for restoring to editor when user aborts.
     * @returns Object with steering and followUp arrays
     */
    clearQueue() {
        const steering = [...this._steeringMessages];
        const followUp = [...this._followUpMessages];
        this._steeringMessages = [];
        this._followUpMessages = [];
        this.agent.clearAllQueues();
        this._emitQueueUpdate();
        return { steering, followUp };
    }
    /** Number of pending messages (includes both steering and follow-up) */
    get pendingMessageCount() {
        return this._steeringMessages.length + this._followUpMessages.length;
    }
    /** Get pending steering messages (read-only) */
    getSteeringMessages() {
        return this._steeringMessages;
    }
    /** Get pending follow-up messages (read-only) */
    getFollowUpMessages() {
        return this._followUpMessages;
    }
    get resourceLoader() {
        return this._resourceLoader;
    }
    /**
     * Abort current operation and wait for agent to become idle.
     */
    async abort() {
        this._parentContinuation.invalidateAll();
        if (this._isAgentRunActive) {
            this._agentRunAbortRequested = true;
        }
        this.abortRetry();
        this.abortCompaction();
        this.abortBranchSummary();
        if (this._isBeforeSettle)
            this._abortDuringBeforeSettle = true;
        this.agent.abort();
        await this.waitForIdle();
    }
    _isIdleAfterNativeCleanup() {
        return (this.isIdle &&
            !this._ownedDispatchActive &&
            !this._nativeDrainPump &&
            !this._isDrainingNativeActions &&
            this._deferredSettledActions.length === 0);
    }
    async waitForIdle() {
        // A drained control command (including Runtime teardown's abort) cannot join
        // the FIFO awaiting that command. This exemption is local to its live action;
        // external callers and detached descendants still join the entire pump.
        // It does not grant input acceptance or support joins from active model hooks.
        const drain = AgentSession._nativeDrainContext.getStore();
        if (drain?.active && drain.owner === this && this.isIdle && !this._ownedDispatchActive)
            return;
        const command = AgentSession._commandTailContext.getStore();
        for (;;) {
            if (!this._isIdleAfterNativeCleanup())
                await this._getIdleWaitPromise();
            // Control descendants join native cleanup, never ancestral or peer control
            // tails. Genuine external callers join both boundaries until stable.
            if (command?.active && command.owner === this)
                break;
            if (this._commandTails.size === 0)
                break;
            await Promise.allSettled([...this._commandTails]);
        }
        if (this._commandTailFailure) {
            const { error } = this._commandTailFailure;
            this._commandTailFailure = undefined;
            throw error;
        }
        if (this._nativeDrainFailure) {
            const { error } = this._nativeDrainFailure;
            this._nativeDrainFailure = undefined;
            throw error;
        }
    }
    // =========================================================================
    // Model Management
    // =========================================================================
    async _emitModelSelect(nextModel, previousModel, source) {
        if (modelsAreEqual(previousModel, nextModel))
            return;
        await this._extensionRunner.emit({
            type: "model_select",
            model: nextModel,
            previousModel,
            source,
        });
    }
    /**
     * Set model directly.
     * Validates that auth is configured and saves to the session transcript.
     * Persists to global defaults only when options.persist is true.
     * @throws Error if no auth is configured for the model
     */
    async setModel(model, options = {}) {
        if (!(await this._modelRuntime.checkAuth(model.provider))) {
            throw new Error(`No API key for ${model.provider}/${model.id}`);
        }
        const previousModel = this.model;
        const thinkingLevel = this._getThinkingLevelForModelSwitch(model);
        this.agent.state.model = model;
        this.sessionManager.appendModelChange(model.provider, model.id);
        if (options.persist) {
            this.settingsManager.setDefaultModelAndProvider(model.provider, model.id);
            this._addPersistedDefaultToNonEmptyScope(model);
        }
        // Apply thinking level for the new model.
        // Per-model thinking level overrides take priority over the global default.
        // Model persistence does not implicitly rewrite the global thinking default.
        this.setThinkingLevel(thinkingLevel);
        await this._emitModelSelect(model, previousModel, "set");
    }
    _addPersistedDefaultToNonEmptyScope(model) {
        if (this._scopedModels.length === 0)
            return;
        if (this._scopedModels.some((scoped) => modelsAreEqual(scoped.model, model)))
            return;
        this._scopedModels = [...this._scopedModels, { model }];
        const enabledModels = this.settingsManager.getEnabledModels();
        if (!enabledModels?.length)
            return;
        const modelReference = `${model.provider}/${model.id}`;
        if (enabledModels.some((pattern) => pattern.toLowerCase() === modelReference.toLowerCase()))
            return;
        this.settingsManager.setEnabledModels([...enabledModels, modelReference]);
    }
    /**
     * Cycle to next/previous model.
     * Uses scoped models (from --models flag) if available, otherwise all available models.
     * @param direction - "forward" (default) or "backward"
     * @returns The new model info, or undefined if only one model available
     */
    async cycleModel(direction = "forward", options = {}) {
        if (this._scopedModels.length > 0) {
            return this._cycleScopedModel(direction, options);
        }
        return this._cycleAvailableModel(direction, options);
    }
    async _cycleScopedModel(direction, options) {
        const availableIds = new Set(this._modelRuntime.getAvailableSnapshot().map((model) => `${model.provider}\0${model.id}`));
        const scopedModels = this._scopedModels.filter((scoped) => availableIds.has(`${scoped.model.provider}\0${scoped.model.id}`));
        if (scopedModels.length <= 1)
            return undefined;
        const currentModel = this.model;
        let currentIndex = scopedModels.findIndex((sm) => modelsAreEqual(sm.model, currentModel));
        if (currentIndex === -1)
            currentIndex = 0;
        const len = scopedModels.length;
        const nextIndex = direction === "forward" ? (currentIndex + 1) % len : (currentIndex - 1 + len) % len;
        const next = scopedModels[nextIndex];
        const thinkingLevel = this._getThinkingLevelForModelSwitch(next.model, next.thinkingLevel);
        // Apply model
        this.agent.state.model = next.model;
        this.sessionManager.appendModelChange(next.model.provider, next.model.id);
        if (options.persist) {
            this.settingsManager.setDefaultModelAndProvider(next.model.provider, next.model.id);
            this._addPersistedDefaultToNonEmptyScope(next.model);
        }
        // Apply thinking level for the new model.
        // - Explicit scoped model thinking level overrides defaults
        // - Per-model thinking level overrides take priority over the global default
        // setThinkingLevel clamps to model capabilities.
        // Model persistence does not implicitly rewrite the global thinking default.
        this.setThinkingLevel(thinkingLevel);
        await this._emitModelSelect(next.model, currentModel, "cycle");
        return { model: next.model, thinkingLevel: this.thinkingLevel, isScoped: true };
    }
    async _cycleAvailableModel(direction, options) {
        const availableModels = this._modelRuntime.getAvailableSnapshot();
        if (availableModels.length <= 1)
            return undefined;
        const currentModel = this.model;
        let currentIndex = availableModels.findIndex((m) => modelsAreEqual(m, currentModel));
        if (currentIndex === -1)
            currentIndex = 0;
        const len = availableModels.length;
        const nextIndex = direction === "forward" ? (currentIndex + 1) % len : (currentIndex - 1 + len) % len;
        const nextModel = availableModels[nextIndex];
        const thinkingLevel = this._getThinkingLevelForModelSwitch(nextModel);
        this.agent.state.model = nextModel;
        this.sessionManager.appendModelChange(nextModel.provider, nextModel.id);
        if (options.persist) {
            this.settingsManager.setDefaultModelAndProvider(nextModel.provider, nextModel.id);
            this._addPersistedDefaultToNonEmptyScope(nextModel);
        }
        // Apply thinking level for the new model.
        // Model persistence does not implicitly rewrite the global thinking default.
        this.setThinkingLevel(thinkingLevel);
        await this._emitModelSelect(nextModel, currentModel, "cycle");
        return { model: nextModel, thinkingLevel: this.thinkingLevel, isScoped: false };
    }
    // =========================================================================
    // Thinking Level Management
    // =========================================================================
    /**
     * Set thinking level.
     * Clamps to model capabilities based on available thinking levels.
     * Saves the clamped level to the session transcript only if the level actually changes.
     * Persists the requested level to global defaults only when options.persist is true.
     */
    setThinkingLevel(level, options = {}) {
        const availableLevels = this.getAvailableThinkingLevels();
        const effectiveLevel = availableLevels.includes(level) ? level : this._clampThinkingLevel(level, availableLevels);
        // Only persist if actually changing
        const previousLevel = this.agent.state.thinkingLevel;
        const isChanging = effectiveLevel !== previousLevel;
        this.agent.state.thinkingLevel = effectiveLevel;
        if (options.persist) {
            this.settingsManager.setDefaultThinkingLevel(level);
        }
        if (isChanging) {
            this.sessionManager.appendThinkingLevelChange(effectiveLevel);
            this._emit({ type: "thinking_level_changed", level: effectiveLevel });
            void this._extensionRunner.emit({
                type: "thinking_level_select",
                level: effectiveLevel,
                previousLevel,
            });
        }
    }
    /**
     * Cycle to next thinking level.
     * @returns New level, or undefined if model doesn't support thinking
     */
    cycleThinkingLevel(options = {}) {
        if (!this.supportsThinking())
            return undefined;
        const levels = this.getAvailableThinkingLevels();
        const currentIndex = levels.indexOf(this.thinkingLevel);
        const nextIndex = (currentIndex + 1) % levels.length;
        const nextLevel = levels[nextIndex];
        this.setThinkingLevel(nextLevel, options);
        return nextLevel;
    }
    /**
     * Get available thinking levels for current model.
     * The provider will clamp to what the specific model supports internally.
     */
    getAvailableThinkingLevels() {
        if (!this.model)
            return [...THINKING_LEVEL_OPTIONS];
        return getSupportedThinkingLevels(this.model);
    }
    /**
     * Check if current model supports thinking/reasoning.
     */
    supportsThinking() {
        return !!this.model?.reasoning;
    }
    _getThinkingLevelForModelSwitch(targetModel, explicitLevel) {
        if (explicitLevel !== undefined) {
            return explicitLevel;
        }
        // Per-model default takes priority when switching to a model that has one
        if (targetModel) {
            const perModel = this.settingsManager.getModelThinkingLevel(targetModel.provider, targetModel.id);
            if (perModel !== undefined) {
                return perModel;
            }
        }
        return this.settingsManager.getDefaultThinkingLevel() ?? this.thinkingLevel ?? DEFAULT_THINKING_LEVEL;
    }
    _clampThinkingLevel(level, _availableLevels) {
        return this.model ? clampThinkingLevel(this.model, level) : "off";
    }
    // =========================================================================
    // Queue Mode Management
    // =========================================================================
    syncQueueModesFromSettings() {
        this.agent.steeringMode = this.settingsManager.getSteeringMode();
        this.agent.followUpMode = this.settingsManager.getFollowUpMode();
    }
    /**
     * Set steering message mode.
     * Saves to settings.
     */
    setSteeringMode(mode) {
        this.agent.steeringMode = mode;
        this.settingsManager.setSteeringMode(mode);
    }
    /**
     * Set follow-up message mode.
     * Saves to settings.
     */
    setFollowUpMode(mode) {
        this.agent.followUpMode = mode;
        this.settingsManager.setFollowUpMode(mode);
    }
    // =========================================================================
    // Compaction
    // =========================================================================
    /** Generate Pi's built-in compaction summary for manual and automatic compaction. */
    async _runDefaultCompaction(preparation, model, customInstructions, signal, reason) {
        // Resolve the request only when Pi summarizes itself: routing may call models or fail.
        const request = await this._getSummarizationRequestAuth(model, signal);
        return compact(preparation, request.model, request.apiKey, request.headers, customInstructions, signal, request.thinkingLevel, this.agent.streamFunction, request.env, this.settingsManager.getRetrySettings(), this._summarizationRetryCallbacks({ source: "compaction", reason }), undefined);
    }
    _clearManualCompactionState() {
        this._compactionAbortController = undefined;
        this._resolveIdleWaitIfIdle();
    }
    /**
     * Manually compact the session context.
     *
     * This is the manual entry point used by `/compact`, RPC, and extensions. It is
     * separate from automatic threshold/overflow compaction, which enters through
     * `_checkCompaction()` and `_runAutoCompaction()`. After preparation and the
     * `session_before_compact` hook, both paths call the lower-level `compact()`
     * function imported from `./compaction/index.ts`, unless the hook cancels or
     * supplies a custom result.
     *
     * Aborts the current agent operation first. Manual compaction never retries or
     * continues the interrupted agent turn.
     *
     * @param customInstructions Optional instructions for the compaction summary
     */
    async compact(customInstructions) {
        const releaseContinuation = this.beginContinuationTransition();
        try {
            await this.abort();
            this._compactionAbortController = new AbortController();
            this._emit({ type: "compaction_start", reason: "manual" });
            let fromExtension = false;
            let cancelledByExtension = false;
            try {
                const model = this.model;
                if (!model) {
                    throw new Error(formatNoModelSelectedMessage());
                }
                const settings = this.settingsManager.getCompactionSettings(model);
                const pathEntries = this.sessionManager.getBranch();
                const preparation = prepareCompaction(pathEntries, settings);
                if (!preparation) {
                    // Check why we can't compact
                    const lastEntry = pathEntries[pathEntries.length - 1];
                    if (lastEntry?.type === "compaction") {
                        throw new Error("Already compacted");
                    }
                    throw new Error("Nothing to compact (session too small)");
                }
                let extensionCompaction;
                if (this._extensionRunner.hasHandlers("session_before_compact")) {
                    const result = (await this._extensionRunner.emit({
                        type: "session_before_compact",
                        preparation,
                        branchEntries: pathEntries,
                        customInstructions,
                        reason: "manual",
                        willRetry: false,
                        signal: this._compactionAbortController.signal,
                    }));
                    if (result?.cancel) {
                        cancelledByExtension = true;
                        throw new Error("Compaction cancelled");
                    }
                    if (result?.compaction) {
                        extensionCompaction = result.compaction;
                        fromExtension = true;
                    }
                }
                let summary;
                let firstKeptEntryId;
                let tokensBefore;
                let usage;
                let details;
                if (extensionCompaction) {
                    // Extension provided compaction content
                    summary = extensionCompaction.summary;
                    firstKeptEntryId = extensionCompaction.firstKeptEntryId;
                    tokensBefore = extensionCompaction.tokensBefore;
                    usage = extensionCompaction.usage;
                    details = extensionCompaction.details;
                }
                else {
                    // Shared default summary generator, also used by automatic compaction.
                    const result = await this._runDefaultCompaction(preparation, model, customInstructions, this._compactionAbortController.signal, "manual");
                    summary = result.summary;
                    firstKeptEntryId = result.firstKeptEntryId;
                    tokensBefore = result.tokensBefore;
                    usage = result.usage;
                    details = result.details;
                }
                if (this._compactionAbortController.signal.aborted) {
                    throw new Error("Compaction cancelled");
                }
                this.sessionManager.appendCompaction(summary, firstKeptEntryId, tokensBefore, details, fromExtension, usage);
                const newEntries = this.sessionManager.getEntries();
                this._refreshFinalizedContext();
                const estimatedTokensAfter = estimateMessagesTokens(this.sessionManager.buildSessionProjection().messages);
                // Get the saved compaction entry for the extension event
                const savedCompactionEntry = newEntries.find((e) => e.type === "compaction" && e.summary === summary);
                if (this._extensionRunner && savedCompactionEntry) {
                    await this._extensionRunner.emit({
                        type: "session_compact",
                        compactionEntry: savedCompactionEntry,
                        fromExtension,
                        reason: "manual",
                        willRetry: false,
                    });
                }
                const compactionResult = {
                    summary,
                    firstKeptEntryId,
                    tokensBefore,
                    estimatedTokensAfter,
                    usage,
                    details,
                };
                // compaction_end listeners may submit queued prompts, so expose idle state before notifying them.
                this._clearManualCompactionState();
                this._emit({
                    type: "compaction_end",
                    reason: "manual",
                    result: compactionResult,
                    aborted: false,
                    willRetry: false,
                });
                return compactionResult;
            }
            catch (error) {
                const message = error instanceof Error ? error.message : String(error);
                const aborted = this._compactionAbortController?.signal.aborted || cancelledByExtension;
                const errorMessage = aborted ? undefined : `Compaction failed: ${message}`;
                this._clearManualCompactionState();
                this._emit({
                    type: "compaction_end",
                    reason: "manual",
                    result: undefined,
                    aborted,
                    willRetry: false,
                    errorMessage,
                });
                await this._emitSessionCompactFailed({
                    reason: "manual",
                    errorMessage,
                    aborted,
                    willRetry: false,
                    fromExtension,
                });
                throw error;
            }
            finally {
                this._clearManualCompactionState();
            }
        }
        finally {
            releaseContinuation();
        }
    }
    /**
     * Cancel in-progress compaction (manual or auto).
     */
    abortCompaction() {
        this._compactionAbortController?.abort();
        this._autoCompactionAbortController?.abort();
    }
    /**
     * Cancel in-progress branch summarization.
     */
    abortBranchSummary() {
        this._branchSummaryAbortController?.abort();
    }
    /**
     * Dispatch automatic compaction after `agent_end` or before prompt submission.
     * Manual compaction does not call this method; it enters through `compact()`.
     *
     * Automatic cases:
     * 1. Overflow with retry: a context-overflow error or recoverable length stop;
     *    remove the failed assistant message, compact, and retry the turn once.
     * 2. Overflow without retry: a successful response exceeded the configured
     *    context window; compact but preserve the completed response.
     * 3. Threshold without retry: valid or estimated context usage crossed the
     *    configured threshold; compact without retrying the completed response.
     *
     * Each case calls `_runAutoCompaction()`. After preparation and the
     * `session_before_compact` hook, that method calls the lower-level `compact()`
     * function imported from `./compaction/index.ts`, unless the hook cancels or
     * supplies a custom result.
     *
     * @param assistantMessage The assistant message to check
     * @param skipAbortedCheck If false, include aborted messages (for pre-prompt check). Default: true
     * @returns Whether the post-run loop should call `agent.continue()` for overflow recovery or queued messages
     */
    async _checkCompaction(assistantMessage, skipAbortedCheck = true) {
        const settings = this.settingsManager.getCompactionSettings(this.model);
        if (!settings.enabled)
            return false;
        // Skip if message was aborted (user cancelled) - unless skipAbortedCheck is false
        if (skipAbortedCheck && assistantMessage.stopReason === "aborted")
            return false;
        // Skip overflow check if the message came from a different model.
        // This handles the case where user switched from a smaller-context model (e.g. opus)
        // to a larger-context model (e.g. codex) - the overflow error from the old model
        // shouldn't trigger compaction for the new model. Under a virtual selection, the
        // physical model that produced the message supplies the limits.
        const messageModel = this._modelForMessage(assistantMessage);
        const sameModel = messageModel !== undefined;
        const contextWindow = (messageModel ?? this.model)?.contextWindow ?? 0;
        // Skip compaction checks if this assistant message is older than the latest
        // compaction boundary. This prevents a stale pre-compaction usage/error
        // from retriggering compaction on the first prompt after compaction.
        const compactionEntry = getLatestCompactionEntry(this.sessionManager.getBranch());
        const assistantIsFromBeforeCompaction = compactionEntry !== null && assistantMessage.timestamp <= new Date(compactionEntry.timestamp).getTime();
        if (assistantIsFromBeforeCompaction) {
            return false;
        }
        // Automatic cases 1 and 2: context overflow.
        // A length stop is recoverable when output ended below the model's original desired limit,
        // independent of the configured context size or any context-clamped provider request limit.
        const currentProjection = this.sessionManager.buildSessionProjection();
        const assistantEntryId = this._findPersistedMessageEntryId(assistantMessage);
        const assistantIsProjected = assistantEntryId === undefined ||
            currentProjection.entries.some((entry) => entry.sourceEntry.id === assistantEntryId &&
                entry.messages.some((message) => message.role === "assistant"));
        const branch = this.sessionManager.getBranch();
        const assistantIndex = assistantEntryId ? branch.findIndex((entry) => entry.id === assistantEntryId) : -1;
        const entriesAfterAssistant = assistantIndex >= 0 ? branch.slice(assistantIndex + 1) : [];
        const hasPostAssistantContextEdit = entriesAfterAssistant.some((entry) => entry.type === "context_edit");
        const latestAssistantEdit = entriesAfterAssistant
            .filter((entry) => entry.type === "context_edit" && entry.targetId === assistantEntryId)
            .at(-1);
        const assistantRetainedForExplicitRecovery = assistantEntryId === undefined ||
            (!entriesAfterAssistant.some((entry) => entry.type === "compaction") &&
                latestAssistantEdit?.replacement !== null);
        const assistantUsageMatchesProjection = assistantIsProjected && !hasPostAssistantContextEdit;
        const explicitOverflow = assistantMessage.stopReason === "error" && isContextOverflow(assistantMessage);
        const contextOverflow = sameModel &&
            ((explicitOverflow && assistantRetainedForExplicitRecovery) ||
                (assistantUsageMatchesProjection && isContextOverflow(assistantMessage, contextWindow)));
        const recoverableLength = sameModel && assistantIsProjected && isRecoverableLength(assistantMessage, messageModel.maxTokens);
        if (contextOverflow || recoverableLength) {
            const willRetry = assistantMessage.stopReason !== "stop";
            // Case 2: the response completed successfully. Compact, but do not retry because
            // agent.continue() cannot continue from a completed assistant response.
            if (!willRetry) {
                return await this._runAutoCompaction("overflow", false);
            }
            if (this._overflowRecoveryAttempted) {
                const errorMessage = contextOverflow
                    ? "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                    : "Truncated response recovery failed after one compact-and-retry attempt.";
                this._emit({
                    type: "compaction_end",
                    reason: "overflow",
                    result: undefined,
                    aborted: false,
                    willRetry: false,
                    errorMessage,
                });
                await this._emitSessionCompactFailed({
                    reason: "overflow",
                    errorMessage,
                    aborted: false,
                    willRetry: false,
                    fromExtension: false,
                });
                return false;
            }
            // Persistently omit the selected final attempt before post-run recovery compaction.
            this._overflowRecoveryAttempted = true;
            this._omitRecoveryAttempt(assistantMessage);
            const retry = await this._runAutoCompaction("overflow", willRetry);
            if (retry)
                this._failedResponse = assistantMessage;
            return retry;
        }
        // Case 3: threshold compaction without retry.
        // For error messages or all-zero usage messages, estimate from the last valid response.
        // This ensures sessions that hit persistent API errors (e.g. 529) or malformed zero-usage
        // responses can still compact and do not reset context accounting.
        let contextTokens;
        const projection = currentProjection;
        const hasContextEdits = projection.entries.some((entry) => entry.sourceEntry.type === "context_edit");
        const directContextTokens = assistantMessage.usage ? calculateContextTokens(assistantMessage.usage) : 0;
        if (hasContextEdits) {
            contextTokens = estimateProjectedContextTokens(projection, branch).tokens;
        }
        else if (assistantMessage.stopReason === "error" || directContextTokens === 0) {
            const messages = this.agent.state.messages;
            const estimate = estimateContextTokens(messages);
            // Without provider usage, estimate.tokens is the pure message-size estimate.
            // Only usage-backed estimates need the stale pre-compaction check.
            if (estimate.lastUsageIndex !== null) {
                // Verify the usage source is post-compaction. Kept pre-compaction messages
                // have stale usage reflecting the old (larger) context and would falsely
                // trigger compaction right after one just finished.
                const usageMsg = messages[estimate.lastUsageIndex];
                if (compactionEntry &&
                    usageMsg.role === "assistant" &&
                    usageMsg.timestamp <= new Date(compactionEntry.timestamp).getTime()) {
                    return false;
                }
            }
            contextTokens = estimate.tokens;
        }
        else {
            contextTokens = directContextTokens;
        }
        if (shouldCompact(contextTokens, contextWindow, settings)) {
            return await this._runAutoCompaction("threshold", false);
        }
        return false;
    }
    /**
     * Execute threshold or overflow compaction. Manual compaction uses
     * `AgentSession.compact()` instead. Both paths call the lower-level `compact()`
     * function imported from `./compaction/index.ts` after preparation and extension
     * interception.
     *
     * @param reason Automatic trigger selected by `_checkCompaction()`
     * @param willRetry Whether to continue the interrupted turn after overflow compaction
     * @returns Whether the post-run loop should call `agent.continue()`
     */
    async _runAutoCompaction(reason, willRetry) {
        const model = this.model;
        const settings = this.settingsManager.getCompactionSettings(model);
        let abortController;
        let started = false;
        let fromExtension = false;
        let cancelledByExtension = false;
        try {
            if (!model) {
                return false;
            }
            const pathEntries = this.sessionManager.getBranch();
            const preparation = prepareCompaction(pathEntries, settings);
            if (!preparation) {
                return false;
            }
            abortController = new AbortController();
            this._autoCompactionAbortController = abortController;
            started = true;
            this._emit({ type: "compaction_start", reason });
            abortController.signal.throwIfAborted();
            let extensionCompaction;
            if (this._extensionRunner.hasHandlers("session_before_compact")) {
                const extensionResult = (await this._extensionRunner.emit({
                    type: "session_before_compact",
                    preparation,
                    branchEntries: pathEntries,
                    customInstructions: undefined,
                    reason,
                    willRetry,
                    signal: abortController.signal,
                }));
                if (extensionResult?.cancel) {
                    cancelledByExtension = true;
                    throw new Error("Compaction cancelled");
                }
                if (extensionResult?.compaction) {
                    extensionCompaction = extensionResult.compaction;
                    fromExtension = true;
                }
            }
            abortController.signal.throwIfAborted();
            let summary;
            let firstKeptEntryId;
            let tokensBefore;
            let usage;
            let details;
            if (extensionCompaction) {
                // Extension provided compaction content
                summary = extensionCompaction.summary;
                firstKeptEntryId = extensionCompaction.firstKeptEntryId;
                tokensBefore = extensionCompaction.tokensBefore;
                usage = extensionCompaction.usage;
                details = extensionCompaction.details;
            }
            else {
                // Shared default summary generator, also used by manual compaction.
                const compactResult = await this._runDefaultCompaction(preparation, model, undefined, abortController.signal, reason);
                summary = compactResult.summary;
                firstKeptEntryId = compactResult.firstKeptEntryId;
                tokensBefore = compactResult.tokensBefore;
                usage = compactResult.usage;
                details = compactResult.details;
            }
            abortController.signal.throwIfAborted();
            this.sessionManager.appendCompaction(summary, firstKeptEntryId, tokensBefore, details, fromExtension, usage);
            const newEntries = this.sessionManager.getEntries();
            this._refreshFinalizedContext();
            const estimatedTokensAfter = estimateMessagesTokens(this.sessionManager.buildSessionProjection().messages);
            // Get the saved compaction entry for the extension event
            const savedCompactionEntry = newEntries.find((e) => e.type === "compaction" && e.summary === summary);
            if (this._extensionRunner && savedCompactionEntry) {
                await this._extensionRunner.emit({
                    type: "session_compact",
                    compactionEntry: savedCompactionEntry,
                    fromExtension,
                    reason,
                    willRetry,
                });
            }
            const result = {
                summary,
                firstKeptEntryId,
                tokensBefore,
                estimatedTokensAfter,
                usage,
                details,
            };
            this._emit({ type: "compaction_end", reason, result, aborted: false, willRetry });
            if (willRetry)
                return true;
            // Auto-compaction can complete while follow-up/steering/custom messages are waiting.
            // Continue once so queued messages are delivered.
            return this.agent.hasQueuedMessages();
        }
        catch (error) {
            const message = error instanceof Error ? error.message : "compaction failed";
            const aborted = abortController?.signal.aborted === true || cancelledByExtension;
            if (started) {
                const errorMessage = aborted
                    ? undefined
                    : reason === "overflow"
                        ? `Context overflow recovery failed: ${message}`
                        : `Auto-compaction failed: ${message}`;
                this._emit({
                    type: "compaction_end",
                    reason,
                    result: undefined,
                    aborted,
                    willRetry: false,
                    errorMessage,
                });
                await this._emitSessionCompactFailed({
                    reason,
                    errorMessage,
                    aborted,
                    willRetry: false,
                    fromExtension,
                });
            }
            return false;
        }
        finally {
            if (this._autoCompactionAbortController === abortController) {
                this._autoCompactionAbortController = undefined;
            }
            this._resolveIdleWaitIfIdle();
        }
    }
    /**
     * Toggle auto-compaction setting.
     */
    setAutoCompactionEnabled(enabled) {
        this.settingsManager.setCompactionEnabled(enabled);
    }
    /** Whether auto-compaction is enabled */
    get autoCompactionEnabled() {
        return this.settingsManager.getCompactionEnabled();
    }
    async bindExtensions(bindings) {
        this._commandGeneration = {};
        if (bindings.uiContext !== undefined) {
            this._extensionUIContext = bindings.uiContext;
        }
        if (bindings.mode !== undefined) {
            this._extensionMode = bindings.mode;
        }
        if (bindings.commandContextActions !== undefined) {
            this._extensionCommandContextActions = bindings.commandContextActions;
        }
        if (bindings.abortHandler !== undefined) {
            this._extensionAbortHandler = bindings.abortHandler;
        }
        if (bindings.shutdownHandler !== undefined) {
            this._extensionShutdownHandler = bindings.shutdownHandler;
        }
        if (bindings.onError !== undefined) {
            this._extensionErrorListener = bindings.onError;
        }
        this._applyExtensionBindings(this._extensionRunner);
        await this._extensionRunner.emit(this._sessionStartEvent);
        this._extensionRunner.reportUnhandledMcpServers();
        await this.extendResourcesFromExtensions(this._sessionStartEvent.reason === "reload" ? "reload" : "startup");
    }
    async extendResourcesFromExtensions(reason) {
        if (!this._extensionRunner.hasHandlers("resources_discover")) {
            return;
        }
        const { skillPaths, promptPaths, themePaths } = await this._extensionRunner.emitResourcesDiscover(this._cwd, reason);
        if (skillPaths.length === 0 && promptPaths.length === 0 && themePaths.length === 0) {
            return;
        }
        const extensionPaths = {
            skillPaths: this.buildExtensionResourcePaths(skillPaths),
            promptPaths: this.buildExtensionResourcePaths(promptPaths),
            themePaths: this.buildExtensionResourcePaths(themePaths),
        };
        this._resourceLoader.extendResources(extensionPaths);
        this._rebuildSystemPrompt(this.getActiveToolNames());
    }
    buildExtensionResourcePaths(entries) {
        return entries.map((entry) => {
            const source = this.getExtensionSourceLabel(entry.extensionPath);
            const baseDir = isSyntheticPath(entry.extensionPath) ? undefined : dirname(entry.extensionPath);
            return {
                path: entry.path,
                metadata: {
                    source,
                    scope: "temporary",
                    origin: "top-level",
                    baseDir,
                },
            };
        });
    }
    getExtensionSourceLabel(extensionPath) {
        if (isSyntheticPath(extensionPath)) {
            return `extension:${extensionPath.replace(/[<>]/g, "")}`;
        }
        const base = basename(extensionPath);
        const name = base.replace(/\.(ts|js)$/, "");
        return `extension:${name}`;
    }
    _applyExtensionBindings(runner) {
        runner.setUIContext(this._extensionUIContext, this._extensionMode);
        runner.bindCommandContext(this._extensionCommandContextActions);
        this._extensionErrorUnsubscriber?.();
        this._extensionErrorUnsubscriber = this._extensionErrorListener
            ? runner.onError(this._extensionErrorListener)
            : undefined;
    }
    _refreshCurrentModelFromRegistry() {
        const currentModel = this.model;
        if (!currentModel) {
            return;
        }
        const refreshedModel = this._modelRuntime.getModel(currentModel.provider, currentModel.id);
        if (!refreshedModel || refreshedModel === currentModel) {
            return;
        }
        this.agent.state.model = refreshedModel;
    }
    _bindExtensionCore(runner) {
        const getCommands = () => {
            const extensionCommands = runner.getRegisteredCommands().map((command) => ({
                name: command.invocationName,
                description: command.description,
                source: "extension",
                sourceInfo: command.sourceInfo,
            }));
            const templates = this.promptTemplates.map((template) => ({
                name: template.name,
                description: template.description,
                source: "prompt",
                sourceInfo: template.sourceInfo,
            }));
            const skills = this._resourceLoader.getSkills().skills.map((skill) => ({
                name: `skill:${skill.name}`,
                description: skill.description,
                source: "skill",
                sourceInfo: skill.sourceInfo,
            }));
            return [...extensionCommands, ...templates, ...skills];
        };
        runner.bindCore({
            sendMessage: (message, options) => {
                this.sendCustomMessage(message, options).catch((err) => {
                    runner.emitError({
                        extensionPath: "<runtime>",
                        event: "send_message",
                        error: err instanceof Error ? err.message : String(err),
                    });
                });
            },
            sendUserMessage: (content, options) => {
                this.sendUserMessage(content, options).catch((err) => {
                    runner.emitError({
                        extensionPath: "<runtime>",
                        event: "send_user_message",
                        error: err instanceof Error ? err.message : String(err),
                    });
                });
            },
            appendEntry: (customType, data) => {
                const entryId = this.sessionManager.appendCustomEntry(customType, data);
                const entry = this.sessionManager.getEntry(entryId);
                if (entry) {
                    this._emit({ type: "entry_appended", entry });
                }
            },
            setSessionName: (name) => {
                this.setSessionName(name);
            },
            getSessionName: () => {
                return this.sessionManager.getSessionName();
            },
            setLabel: (entryId, label) => {
                this.sessionManager.appendLabelChange(entryId, label);
            },
            getActiveTools: () => this.getActiveToolNames(),
            getAllTools: () => this.getAllTools(),
            getSettings: () => this.settingsManager.getSettings(),
            setActiveTools: (toolNames) => this.setActiveToolsByName(toolNames),
            refreshTools: () => this._refreshToolRegistry(),
            getCommands,
            setModel: async (model) => {
                if (!this._modelRuntime.hasConfiguredAuth(model.provider))
                    return false;
                await this.setModel(model);
                return true;
            },
            getThinkingLevel: () => this.thinkingLevel,
            setThinkingLevel: (level) => this.setThinkingLevel(level),
        }, {
            getModel: () => this.model,
            getScopedModels: () => this._scopedModels,
            parentContinuation: this._parentContinuation,
            isIdle: () => this.isIdle,
            isProjectTrusted: () => this.settingsManager.isProjectTrusted(),
            getSignal: () => this.agent.signal,
            abort: () => {
                if (this._extensionAbortHandler) {
                    this._extensionAbortHandler();
                    return;
                }
                void this.abort();
            },
            hasPendingMessages: () => this.pendingMessageCount > 0,
            shutdown: () => {
                this._extensionShutdownHandler?.();
            },
            getContextUsage: () => this.getContextUsage(),
            compact: (options) => {
                void (async () => {
                    try {
                        const result = await this.compact(options?.customInstructions);
                        options?.onComplete?.(result);
                    }
                    catch (error) {
                        const err = error instanceof Error ? error : new Error(String(error));
                        options?.onError?.(err);
                    }
                })();
            },
            getSystemPrompt: () => this.systemPrompt,
            getSystemPromptOptions: () => this._baseSystemPromptOptions,
            executeTool: (callerId, name, args, options) => this._executeNestedToolCall(callerId, name, args, options),
            getCallableTools: () => this._getCallableTools(),
        }, {
            registerProvider: (name, config) => {
                this._modelRuntime.registerProvider(name, config);
                this._refreshCurrentModelFromRegistry();
            },
            registerNativeProvider: (provider) => {
                this._modelRuntime.registerNativeProvider(provider);
                this._refreshCurrentModelFromRegistry();
            },
            unregisterProvider: (name) => {
                this._modelRuntime.unregisterProvider(name);
                this._refreshCurrentModelFromRegistry();
            },
            registerVirtualModel: (definition) => {
                this._modelRuntime.registerVirtualModel(definition);
                this._refreshCurrentModelFromRegistry();
            },
            unregisterVirtualModel: (provider, id) => {
                this._modelRuntime.unregisterVirtualModel(provider, id);
                this._refreshCurrentModelFromRegistry();
            },
        });
    }
    /** Build private maps. Legacy loader enumeration is live; owned updates must supply a provisional source. */
    _prepareToolRegistry(revision, extensionSources = this._extensionRunner.getAllRegisteredTools()) {
        const definitions = new Map();
        const sources = [
            ...Array.from(this._baseToolDefinitions, ([name, definition]) => ({
                definition,
                sourceInfo: createSyntheticSourceInfo(`${BUILTIN_PATH_PREFIX}${name}`, { source: "builtin" }),
            })),
            ...extensionSources,
            ...this._customTools.map((definition) => ({
                definition,
                sourceInfo: createSyntheticSourceInfo(`<sdk:${definition.name}>`, { source: "sdk" }),
            })),
        ];
        if (revision !== this._toolSelectionRevision)
            return undefined;
        for (const entry of sources) {
            // Copy values without freezing or changing caller definitions. Check between accessor reads.
            const keys = new Set();
            const seen = new Set();
            let prototype = entry.definition;
            while (prototype && prototype !== Object.prototype && !seen.has(prototype)) {
                seen.add(prototype);
                const ownKeys = Object.getOwnPropertyNames(prototype);
                if (revision !== this._toolSelectionRevision)
                    return undefined;
                for (const key of ownKeys)
                    keys.add(key);
                prototype = Object.getPrototypeOf(prototype);
                if (revision !== this._toolSelectionRevision)
                    return undefined;
            }
            const values = {};
            for (const key of keys) {
                const value = Reflect.get(entry.definition, key);
                if (revision !== this._toolSelectionRevision)
                    return undefined;
                Object.defineProperty(values, key, { value, enumerable: true, writable: true, configurable: true });
            }
            const definition = values;
            const allowed = this._isAllowedTool(definition.name);
            if (revision !== this._toolSelectionRevision)
                return undefined;
            if (allowed)
                definitions.set(definition.name, { definition, sourceInfo: entry.sourceInfo });
        }
        const snippets = new Map();
        const guidelines = new Map();
        for (const { definition } of definitions.values()) {
            const snippet = this._normalizePromptSnippet(definition.promptSnippet);
            if (revision !== this._toolSelectionRevision)
                return undefined;
            const rules = this._normalizePromptGuidelines(definition.promptGuidelines);
            if (revision !== this._toolSelectionRevision)
                return undefined;
            if (snippet)
                snippets.set(definition.name, snippet);
            if (rules.length)
                guidelines.set(definition.name, rules);
        }
        const wrapped = wrapRegisteredTools([...definitions.values()], this._extensionRunner);
        if (revision !== this._toolSelectionRevision)
            return undefined;
        return { registry: new Map(wrapped.map((tool) => [tool.name, tool])), definitions, snippets, guidelines };
    }
    _refreshToolRegistry(options) {
        const revision = ++this._toolSelectionRevision;
        const previousActivatedOnRegistration = new Set();
        for (const name of this._toolRegistry.keys()) {
            const activated = this._isActivatedOnRegistration(name);
            if (revision !== this._toolSelectionRevision)
                return;
            if (activated)
                previousActivatedOnRegistration.add(name);
        }
        const previousActiveToolNames = this.getActiveToolNames();
        const context = this._prepareToolRegistry(revision);
        if (!context)
            return;
        const nextActiveToolNames = [];
        for (const name of options?.activeToolNames ?? previousActiveToolNames) {
            const allowed = this._isAllowedTool(name);
            if (revision !== this._toolSelectionRevision)
                return;
            if (allowed)
                nextActiveToolNames.push(name);
        }
        for (const name of context.registry.keys()) {
            let activate = false;
            if (this._allowedTools) {
                // Legacy refresh deliberately reactivates allowlisted foreign tools.
                activate = this._allowedTools(name) && this._isDeclarable(name, context);
            }
            else if (options?.includeAllExtensionTools) {
                activate =
                    context.definitions.get(name)?.sourceInfo.source !== "builtin" &&
                        this._isActivatedOnRegistration(name, context);
            }
            else if (!options?.activeToolNames) {
                activate = !previousActivatedOnRegistration.has(name) && this._isActivatedOnRegistration(name, context);
            }
            if (revision !== this._toolSelectionRevision)
                return;
            if (activate)
                nextActiveToolNames.push(name);
        }
        nextActiveToolNames.push(...this._pendingToolNames);
        this._setActiveTools([...new Set(nextActiveToolNames)], false, context, revision);
    }
    /** Whether activating the tool declares it to the model. */
    _isDeclarable(name, context = this._committedToolRegistry()) {
        const exposure = this._getToolExposure(name, context);
        return exposure === "direct" || exposure === "model-only";
    }
    /** Whether registering the tool activates it, which declares it to the model. */
    _isActivatedOnRegistration(name, context = this._committedToolRegistry()) {
        return this._isDeclarable(name, context) && context.definitions.get(name)?.definition.defaultActive !== false;
    }
    _buildRuntime(options) {
        const autoResizeImages = this.settingsManager.getImageAutoResize();
        const shellCommandPrefix = this.settingsManager.getShellCommandPrefix();
        const shellPath = this.settingsManager.getShellPath();
        const baseToolDefinitions = this._baseToolsOverride
            ? Object.fromEntries(Object.entries(this._baseToolsOverride).map(([name, tool]) => [
                name,
                createToolDefinitionFromAgentTool(tool),
            ]))
            : createAllToolDefinitions(this._cwd, {
                read: { autoResizeImages },
                bash: { commandPrefix: shellCommandPrefix, shellPath },
            });
        this._baseToolDefinitions = new Map(Object.entries(baseToolDefinitions).map(([name, tool]) => [name, tool]));
        const extensionsResult = this._resourceLoader.getExtensions();
        if (options.flagValues) {
            for (const [name, value] of options.flagValues) {
                extensionsResult.runtime.flagValues.set(name, value);
            }
        }
        this._extensionRunner = new ExtensionRunner(extensionsResult.extensions, extensionsResult.runtime, this._cwd, this.sessionManager, new ModelRegistry(this._modelRuntime));
        if (this._extensionRunnerRef) {
            this._extensionRunnerRef.current = this._extensionRunner;
        }
        bindToolRegistryWriter(extensionsResult.runtime, () => {
            const revision = ++this._toolSelectionRevision;
            return () => revision === this._toolSelectionRevision;
        });
        this._bindExtensionCore(this._extensionRunner);
        bindAtomicOwnedToolsBackend(extensionsResult.runtime, {
            collides: (owner, names) => [...names].some((name) => this._baseToolDefinitions.has(name) ||
                this._customTools.some((tool) => tool.name === name) ||
                extensionsResult.extensions.some((extension) => extension !== owner && extension.tools.has(name))),
            update: (owner, batch, current) => {
                const revision = ++this._toolSelectionRevision;
                try {
                    const provisional = new Map(owner.tools);
                    for (const { definition, activation } of batch) {
                        if (typeof definition.name !== "string" ||
                            !definition.name ||
                            typeof definition.label !== "string" ||
                            typeof definition.description !== "string" ||
                            typeof definition.execute !== "function" ||
                            (definition.exposure !== undefined &&
                                !["direct", "model-only", "codemode", "deferred", "hidden"].includes(definition.exposure)) ||
                            !definition.parameters ||
                            typeof definition.parameters !== "object" ||
                            Array.isArray(definition.parameters) ||
                            !["preserve", "activate", "deactivate", "default"].includes(activation)) {
                            return { status: "aborted", reason: "invalid" };
                        }
                        provisional.set(definition.name, { definition, sourceInfo: owner.sourceInfo });
                    }
                    const sources = new Map();
                    for (const extension of extensionsResult.extensions) {
                        for (const [name, entry] of extension === owner ? provisional : extension.tools) {
                            if (!sources.has(name))
                                sources.set(name, entry);
                        }
                    }
                    const context = this._prepareToolRegistry(revision, [...sources.values()]);
                    if (!context)
                        return { status: "aborted", reason: "conflict" };
                    let names = this.getActiveToolNames();
                    for (const { definition, activation } of batch) {
                        const name = definition.name;
                        if (activation === "deactivate" || activation === "default")
                            names = names.filter((item) => item !== name);
                        if (activation === "activate" ||
                            (activation === "default" && this._isActivatedOnRegistration(name, context))) {
                            if (!names.includes(name))
                                names.push(name);
                        }
                    }
                    const candidate = this._prepareToolSelection(names, false, context, revision, true);
                    if (!candidate || !current() || !this._commitToolSelection(candidate, revision))
                        return { status: "aborted", reason: "conflict" };
                    owner.tools = provisional;
                    return { status: "committed" };
                }
                catch {
                    return {
                        status: "aborted",
                        reason: revision === this._toolSelectionRevision && current() ? "prepare-failed" : "conflict",
                    };
                }
            },
        });
        this._applyExtensionBindings(this._extensionRunner);
        const defaultActiveToolNames = this._baseToolsOverride
            ? Object.keys(this._baseToolsOverride)
            : ["read", "bash", "edit", "write"];
        const baseActiveToolNames = options.activeToolNames ?? defaultActiveToolNames;
        this._refreshToolRegistry({
            activeToolNames: baseActiveToolNames,
            includeAllExtensionTools: options.includeAllExtensionTools,
        });
    }
    async reload(options) {
        const releaseContinuation = this.beginContinuationTransition();
        try {
            const oldRunner = this._extensionRunner;
            const previousFlagValues = oldRunner.getFlagValues();
            await emitSessionShutdownEvent(oldRunner, { type: "session_shutdown", reason: "reload" });
            oldRunner.invalidate();
            const getDefaultTools = () => this._usesDefaultTools
                ? applyToolModifiers(this.settingsManager.getDefaultTools() ?? DEFAULT_TOOL_NAMES, this._defaultToolModifiers)
                : [];
            const previousDefaultTools = new Set(getDefaultTools());
            await this.settingsManager.reload();
            this.syncQueueModesFromSettings();
            resetApiProviders();
            await this._resourceLoader.reload();
            // Activate tools newly added to defaultTools. Removed ones stay active, and tools disabled
            // during the session stay disabled unless the setting newly adds them.
            const addedDefaultTools = getDefaultTools().filter((name) => !previousDefaultTools.has(name));
            // Tools the new extensions register later, such as MCP tools, are pending until then.
            for (const name of this.getActiveToolNames())
                this._pendingToolNames.add(name);
            this._buildRuntime({
                activeToolNames: [...this.getActiveToolNames(), ...addedDefaultTools],
                flagValues: previousFlagValues,
                includeAllExtensionTools: true,
            });
            const hasBindings = this._extensionUIContext ||
                this._extensionCommandContextActions ||
                this._extensionShutdownHandler ||
                this._extensionErrorListener;
            if (hasBindings) {
                await options?.beforeSessionStart?.();
                await this._extensionRunner.emit({ type: "session_start", reason: "reload" });
                this._extensionRunner.reportUnhandledMcpServers();
                await this.extendResourcesFromExtensions("reload");
            }
        }
        finally {
            releaseContinuation();
        }
    }
    // =========================================================================
    // Auto-Retry
    // =========================================================================
    /**
     * Check if an error is retryable (overloaded, rate limit, server errors).
     * Context overflow errors are NOT retryable (handled by compaction instead).
     */
    _isRetryableError(message) {
        // Context overflow is handled by compaction, not retry.
        if (isContextOverflow(message, (this._modelForMessage(message) ?? this.model)?.contextWindow ?? 0))
            return false;
        return isRetryableAssistantError(message);
    }
    /**
     * Retry policy + callbacks shared by compaction and branch-summary summarization calls.
     * Uses the same `settings.retry` budget/backoff as agent-turn retries so a single transient
     * stream drop no longer fails the whole operation. `source` carries the context
     * the TUI needs to render the retry and recreate the underlying indicator.
     */
    _summarizationRetryCallbacks(source) {
        return {
            onRetryScheduled: (attempt, maxAttempts, delayMs, errorMessage) => {
                this._emit({
                    type: "summarization_retry_scheduled",
                    attempt,
                    maxAttempts,
                    delayMs,
                    errorMessage,
                });
            },
            onRetryAttemptStart: () => {
                this._emit({
                    type: "summarization_retry_attempt_start",
                    ...source,
                });
            },
            onRetryFinished: () => {
                this._emit({ type: "summarization_retry_finished" });
            },
        };
    }
    _finishCancelledRetry() {
        if (this._retryAttempt === 0)
            return;
        const attempt = this._retryAttempt;
        this._retryAttempt = 0;
        this._emit({
            type: "auto_retry_end",
            success: false,
            attempt,
            finalError: "Retry cancelled",
        });
    }
    /**
     * Prepare a retryable error for continuation with exponential backoff.
     * @returns true if the caller should continue the agent, false otherwise
     */
    async _prepareRetry(message) {
        const settings = this.settingsManager.getRetrySettings();
        if (!settings.enabled) {
            return false;
        }
        this._retryAttempt++;
        if (this._retryAttempt > settings.maxRetries) {
            // Preserve the completed attempt count so post-run handling can emit the final failure.
            this._retryAttempt--;
            return false;
        }
        const delayMs = retryDelayMs(settings, this._retryAttempt);
        const controller = new AbortController();
        this._retryAbortController = controller;
        try {
            this._emit({
                type: "auto_retry_start",
                attempt: this._retryAttempt,
                maxAttempts: settings.maxRetries,
                delayMs,
                errorMessage: message.errorMessage || "Unknown error",
            });
            if (controller.signal.aborted || this._agentRunAbortRequested) {
                this._finishCancelledRetry();
                return false;
            }
            // Keep raw history while durably omitting the failed attempt from model projection.
            this._omitRecoveryAttempt(message);
            try {
                await sleep(delayMs, controller.signal);
            }
            catch {
                this._finishCancelledRetry();
                return false;
            }
            if (controller.signal.aborted || this._agentRunAbortRequested) {
                this._finishCancelledRetry();
                return false;
            }
            return true;
        }
        finally {
            if (this._retryAbortController === controller)
                this._retryAbortController = undefined;
        }
    }
    /**
     * Cancel in-progress retry.
     */
    abortRetry() {
        this._retryAbortController?.abort();
    }
    /** Whether auto-retry is currently in progress */
    get isRetrying() {
        return this._retryAbortController !== undefined;
    }
    /** Whether auto-retry is enabled */
    get autoRetryEnabled() {
        return this.settingsManager.getRetryEnabled();
    }
    /**
     * Toggle auto-retry setting.
     */
    setAutoRetryEnabled(enabled) {
        this.settingsManager.setRetryEnabled(enabled);
    }
    // =========================================================================
    // Bash Execution
    // =========================================================================
    /**
     * Execute a bash command.
     * Adds result to agent context and session.
     * @param command The bash command to execute
     * @param onChunk Optional streaming callback for output
     * @param options.excludeFromContext If true, command output won't be sent to LLM (!! prefix)
     * @param options.id Optional identifier included in bash execution update events
     * @param options.operations Custom BashOperations for remote execution
     */
    async executeBash(command, onChunk, options) {
        const abortController = new AbortController();
        this._bashAbortControllers.add(abortController);
        // Apply command prefix if configured (e.g., "shopt -s expand_aliases" for alias support)
        const prefix = this.settingsManager.getShellCommandPrefix();
        const shellPath = this.settingsManager.getShellPath();
        const resolvedCommand = prefix ? `${prefix}\n${command}` : command;
        try {
            const result = await executeBashWithOperations(resolvedCommand, this.sessionManager.getCwd(), options?.operations ?? createLocalBashOperations({ shellPath }), {
                onChunk: (delta) => {
                    onChunk?.(delta);
                    this._emit({ type: "bash_execution_update", id: options?.id, delta });
                },
                signal: abortController.signal,
            });
            this.recordBashResult(command, result, options);
            return result;
        }
        finally {
            this._bashAbortControllers.delete(abortController);
        }
    }
    /**
     * Record a bash execution result in session history.
     * Used by executeBash and by extensions that handle bash execution themselves.
     */
    recordBashResult(command, result, options) {
        const bashMessage = {
            role: "bashExecution",
            command,
            output: result.output,
            exitCode: result.exitCode,
            cancelled: result.cancelled,
            truncated: result.truncated,
            fullOutputPath: result.fullOutputPath,
            timestamp: Date.now(),
            excludeFromContext: options?.excludeFromContext,
        };
        // If agent is streaming, defer adding to avoid breaking tool_use/tool_result ordering
        if (this.isStreaming) {
            // Queue for later - will be flushed on agent_end
            this._pendingBashMessages.push(bashMessage);
        }
        else {
            this.sessionManager.appendMessage(bashMessage);
            this._refreshFinalizedContext();
        }
    }
    /**
     * Cancel running bash command.
     */
    abortBash() {
        for (const abortController of [...this._bashAbortControllers]) {
            abortController.abort();
        }
    }
    /** Whether a bash command is currently running */
    get isBashRunning() {
        return this._bashAbortControllers.size > 0;
    }
    /** Whether there are pending bash messages waiting to be flushed */
    get hasPendingBashMessages() {
        return this._pendingBashMessages.length > 0;
    }
    /**
     * Flush pending bash messages to agent state and session.
     * Called after agent turn completes to maintain proper message ordering.
     */
    _flushPendingBashMessages() {
        if (this._pendingBashMessages.length === 0)
            return;
        for (const bashMessage of this._pendingBashMessages) {
            this.sessionManager.appendMessage(bashMessage);
        }
        this._pendingBashMessages = [];
        this._refreshFinalizedContext();
    }
    // =========================================================================
    // Session Management
    // =========================================================================
    /**
     * Set a display name for the current session.
     */
    setSessionName(name) {
        this.sessionManager.appendSessionInfo(name);
        const event = { type: "session_info_changed", name: this.sessionManager.getSessionName() };
        this._emit(event);
        void this._extensionRunner.emit(event);
    }
    // =========================================================================
    // Tree Navigation
    // =========================================================================
    /**
     * Navigate to a different node in the session tree.
     * Unlike fork() which creates a new session file, this stays in the same file.
     *
     * @param targetId The entry ID to navigate to
     * @param options.summarize Whether user wants to summarize abandoned branch
     * @param options.customInstructions Custom instructions for summarizer
     * @param options.replaceInstructions If true, customInstructions replaces the default prompt
     * @param options.label Label to attach to the branch summary entry
     * @returns Result with editorText (if user message) and cancelled status
     */
    async navigateTree(targetId, options = {}) {
        const releaseContinuation = this.beginContinuationTransition();
        try {
            if (this.isStreaming) {
                throw new Error("Wait for the current response to finish before navigating the session tree.");
            }
            if (this.isCompacting) {
                throw new Error("Wait for the current compaction or tree navigation to finish before navigating the session tree.");
            }
            const oldLeafId = this.sessionManager.getLeafId();
            // No-op if already at target
            if (targetId === oldLeafId) {
                return { cancelled: false };
            }
            // Model required for summarization
            if (options.summarize && !this.model) {
                throw new Error("No model available for summarization");
            }
            const targetEntry = this.sessionManager.getEntry(targetId);
            if (!targetEntry) {
                throw new Error(`Entry ${targetId} not found`);
            }
            // Collect entries to summarize (from old leaf to common ancestor)
            const { entries: entriesToSummarize, commonAncestorId } = collectEntriesForBranchSummary(this.sessionManager, oldLeafId, targetId);
            // Prepare event data - mutable so extensions can override
            let customInstructions = options.customInstructions;
            let replaceInstructions = options.replaceInstructions;
            let label = options.label;
            const preparation = {
                targetId,
                oldLeafId,
                commonAncestorId,
                entriesToSummarize,
                userWantsSummary: options.summarize ?? false,
                customInstructions,
                replaceInstructions,
                label,
            };
            // Set up abort controller for summarization
            this._branchSummaryAbortController = new AbortController();
            try {
                let extensionSummary;
                let fromExtension = false;
                // Emit session_before_tree event
                if (this._extensionRunner.hasHandlers("session_before_tree")) {
                    const result = (await this._extensionRunner.emit({
                        type: "session_before_tree",
                        preparation,
                        signal: this._branchSummaryAbortController.signal,
                    }));
                    if (result?.cancel) {
                        return { cancelled: true };
                    }
                    if (result?.summary && options.summarize) {
                        extensionSummary = result.summary;
                        fromExtension = true;
                    }
                    // Allow extensions to override instructions and label
                    if (result?.customInstructions !== undefined) {
                        customInstructions = result.customInstructions;
                    }
                    if (result?.replaceInstructions !== undefined) {
                        replaceInstructions = result.replaceInstructions;
                    }
                    if (result?.label !== undefined) {
                        label = result.label;
                    }
                }
                // Run default summarizer if needed
                let summaryText;
                let summaryDetails;
                let summaryUsage;
                if (options.summarize && entriesToSummarize.length > 0 && !extensionSummary) {
                    const signal = this._branchSummaryAbortController.signal;
                    const branchSummarySettings = this.settingsManager.getBranchSummarySettings();
                    const result = await generateBranchSummary(entriesToSummarize, {
                        ...(await this._getSummarizationRequestAuth(this.model, signal)),
                        signal,
                        customInstructions,
                        replaceInstructions,
                        reserveTokens: branchSummarySettings.reserveTokens,
                        streamFn: this.agent.streamFunction,
                        retry: this.settingsManager.getRetrySettings(),
                        callbacks: this._summarizationRetryCallbacks({ source: "branchSummary" }),
                    });
                    if (result.aborted) {
                        return { cancelled: true, aborted: true };
                    }
                    if (result.error) {
                        throw new Error(result.error);
                    }
                    summaryText = result.summary;
                    summaryUsage = result.usage;
                    summaryDetails = {
                        readFiles: result.readFiles || [],
                        modifiedFiles: result.modifiedFiles || [],
                    };
                }
                else if (extensionSummary) {
                    summaryText = extensionSummary.summary;
                    summaryDetails = extensionSummary.details;
                    summaryUsage = extensionSummary.usage;
                }
                // Determine the new leaf position based on target type
                let newLeafId;
                let editorText;
                if (targetEntry.type === "message" && targetEntry.message.role === "user") {
                    // User message: leaf = parent (null if root), text goes to editor
                    newLeafId = targetEntry.parentId;
                    editorText = contentText(targetEntry.message.content, "");
                }
                else if (targetEntry.type === "custom_message") {
                    // Custom message: leaf = parent (null if root), text goes to editor
                    newLeafId = targetEntry.parentId;
                    editorText = contentText(targetEntry.content, "");
                }
                else {
                    // Non-user message: leaf = selected node
                    newLeafId = targetId;
                }
                // Switch leaf (with or without summary)
                // Summary is attached at the navigation target position (newLeafId), not the old branch
                let summaryEntry;
                if (summaryText) {
                    // Create summary at target position (can be null for root)
                    const summaryId = this.sessionManager.branchWithSummary(newLeafId, summaryText, summaryDetails, fromExtension, summaryUsage);
                    summaryEntry = this.sessionManager.getEntry(summaryId);
                    // Attach label to the summary entry
                    if (label) {
                        this.sessionManager.appendLabelChange(summaryId, label);
                    }
                }
                else if (newLeafId === null) {
                    // No summary, navigating to root - reset leaf
                    this.sessionManager.resetLeaf();
                }
                else {
                    // No summary, navigating to non-root
                    this.sessionManager.branch(newLeafId);
                }
                // Attach label to target entry when not summarizing (no summary entry to label)
                if (label && !summaryText) {
                    this.sessionManager.appendLabelChange(targetId, label);
                }
                // Update finalized context from the canonical session projection.
                this._refreshFinalizedContext();
                this._restoreToolsFromTranscript();
                // Emit session_tree event
                await this._extensionRunner.emit({
                    type: "session_tree",
                    newLeafId: this.sessionManager.getLeafId(),
                    oldLeafId,
                    summaryEntry,
                    fromExtension: summaryText ? fromExtension : undefined,
                });
                // Emit to custom tools
                return { editorText, cancelled: false, summaryEntry };
            }
            finally {
                this._branchSummaryAbortController = undefined;
                this._resolveIdleWaitIfIdle();
            }
        }
        finally {
            releaseContinuation();
        }
    }
    /**
     * Get all user messages from session for fork selector.
     */
    getUserMessagesForForking() {
        const entries = this.sessionManager.getEntries();
        const result = [];
        for (const entry of entries) {
            if (entry.type !== "message")
                continue;
            if (entry.message.role !== "user")
                continue;
            const text = contentText(entry.message.content, "");
            if (text) {
                result.push({ entryId: entry.id, text });
            }
        }
        return result;
    }
    /**
     * Get session statistics. Aggregates over ALL session entries (including
     * history that was compacted away), so token/cost totals reflect what was
     * actually billed across the session.
     */
    getSessionStats() {
        let userMessages = 0;
        let assistantMessages = 0;
        let toolResults = 0;
        let totalMessages = 0;
        let toolCalls = 0;
        const usageTotals = createUsageTotals();
        for (const entry of this.sessionManager.getEntries()) {
            if (entry.type === "usage") {
                addUsageToTotals(usageTotals, entry.usage);
            }
            else if ((entry.type === "branch_summary" || entry.type === "compaction") && entry.usage) {
                addUsageToTotals(usageTotals, entry.usage);
            }
            if (entry.type !== "message")
                continue;
            totalMessages++;
            const message = entry.message;
            if (message.role === "user") {
                userMessages++;
            }
            else if (message.role === "toolResult") {
                toolResults++;
                if (message.usage) {
                    addUsageToTotals(usageTotals, message.usage);
                }
            }
            else if (message.role === "assistant") {
                assistantMessages++;
                const assistantMsg = message;
                if (Array.isArray(assistantMsg.content)) {
                    toolCalls += assistantMsg.content.filter((c) => c.type === "toolCall").length;
                }
                addUsageToTotals(usageTotals, assistantMsg.usage);
            }
        }
        return {
            sessionFile: this.sessionFile,
            sessionId: this.sessionId,
            userMessages,
            assistantMessages,
            toolCalls,
            toolResults,
            totalMessages,
            tokens: {
                input: usageTotals.input,
                output: usageTotals.output,
                cacheRead: usageTotals.cacheRead,
                cacheWrite: usageTotals.cacheWrite,
                total: usageTotals.input + usageTotals.output + usageTotals.cacheRead + usageTotals.cacheWrite,
            },
            cost: usageTotals.cost,
            contextUsage: this.getContextUsage(),
        };
    }
    getContextUsage() {
        const model = this._limitsModel();
        if (!model)
            return undefined;
        const contextWindow = model.contextWindow ?? 0;
        if (contextWindow <= 0)
            return undefined;
        // After compaction, the last assistant usage reflects pre-compaction context size.
        // We can only trust usage from an assistant that responded after the latest compaction.
        // If no such assistant exists, context token count is unknown until the next LLM response.
        const projection = this.sessionManager.buildSessionProjection();
        const branch = this.sessionManager.getBranch();
        const latestCompaction = getLatestCompactionEntry(branch);
        if (latestCompaction) {
            const projectedAssistants = new Set(projection.entries.flatMap((entry) => entry.messages.some((message) => message.role === "assistant" &&
                message.stopReason !== "aborted" &&
                message.stopReason !== "error" &&
                calculateContextTokens(message.usage) > 0)
                ? [entry.sourceEntry.id]
                : []));
            const compactionIndex = branch.findIndex((entry) => entry.id === latestCompaction.id);
            const hasPostCompactionUsage = branch
                .slice(compactionIndex + 1)
                .some((entry) => projectedAssistants.has(entry.id));
            if (!hasPostCompactionUsage)
                return { tokens: null, contextWindow, percent: null };
        }
        const estimate = estimateProjectedContextTokens(projection, branch);
        const percent = (estimate.tokens / contextWindow) * 100;
        return {
            tokens: estimate.tokens,
            contextWindow,
            percent,
        };
    }
    /**
     * Export session to HTML.
     * @param outputPath Optional output path (defaults to session directory)
     * @param options Optional export presentation settings
     * @returns Path to exported file
     */
    async exportToHtml(outputPath, options = {}) {
        const themeName = [options.themeName, this.settingsManager.getTheme()].find((candidate) => candidate !== undefined && getThemeByName(candidate) !== undefined);
        // Create tool renderer if we have an extension runner (for custom tool HTML rendering)
        const toolRenderer = createToolHtmlRenderer({
            getToolRenderers: (name) => this._extensionRunner.resolveToolRenderers(name, () => this.getToolDefinition(name)),
            theme,
            cwd: this.sessionManager.getCwd(),
        });
        return await exportSessionToHtml(this.sessionManager, this.state, {
            outputPath,
            themeName,
            toolRenderer,
        });
    }
    /**
     * Export the current session branch to a JSONL file.
     * Writes the session header followed by all entries on the current branch path.
     * @param outputPath Target file path. If omitted, generates a timestamped file in cwd.
     * @returns The resolved output file path.
     */
    exportToJsonl(outputPath) {
        return exportSessionToJsonl(this.sessionManager, outputPath);
    }
    /**
     * Ask the current model to describe what went wrong in this session for a bug report.
     * Used when the user declines to share the transcript itself.
     */
    async summarizeForBugReport(options) {
        const model = this.model;
        if (!model) {
            throw new Error("No model selected");
        }
        return generateBugReportSummary({
            ...(await this._getSummarizationRequestAuth(model, options.signal)),
            messages: this.messages,
            hint: options.hint,
            signal: options.signal,
            streamFn: this.agent.streamFunction,
            retry: this.settingsManager.getRetrySettings(),
            sessionId: this.sessionId,
        });
    }
    // =========================================================================
    // Utilities
    // =========================================================================
    /**
     * Get text content of last assistant message.
     * Useful for /copy command.
     * @returns Text content, or undefined if no assistant message exists
     */
    getLastAssistantText() {
        const lastAssistant = this.messages
            .slice()
            .reverse()
            .find((m) => {
            if (m.role !== "assistant")
                return false;
            const msg = m;
            // Skip aborted messages with no content
            if (msg.stopReason === "aborted" && msg.content.length === 0)
                return false;
            return true;
        });
        if (!lastAssistant)
            return undefined;
        let text = "";
        for (const content of lastAssistant.content) {
            if (content.type === "text") {
                text += content.text;
            }
        }
        return text.trim() || undefined;
    }
    // =========================================================================
    // Extension System
    // =========================================================================
    createReplacedSessionContext() {
        const context = Object.defineProperties({}, Object.getOwnPropertyDescriptors(this._extensionRunner.createCommandContext()));
        context.sendMessage = (message, options) => this.sendCustomMessage(message, options);
        context.sendUserMessage = (content, options) => this.sendUserMessage(content, options);
        return context;
    }
    /**
     * Check if extensions have handlers for a specific event type.
     */
    hasExtensionHandlers(eventType) {
        return this._extensionRunner.hasHandlers(eventType);
    }
    /**
     * Get the extension runner (for setting UI context and error handlers).
     */
    get extensionRunner() {
        return this._extensionRunner;
    }
}
