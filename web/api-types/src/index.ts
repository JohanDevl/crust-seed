/**
 * Type-level contract for the crust-seed HTTP API.
 *
 * In cross-seed this package simply re-exported `typeof appRouter` from the
 * TypeScript backend. crust-seed's backend is Rust, so there is no TS router to
 * infer from — this file *is* the contract instead: a declaration-only tRPC
 * router whose procedures have no runtime behaviour, existing only so the
 * vendored React UI keeps its end-to-end types.
 *
 * The Rust server (`src/server/trpc/`) implements the same procedure names,
 * inputs and outputs. Changing a payload means changing it in both places.
 */
import { initTRPC } from "@trpc/server";
import { z } from "zod";
import type { RuntimeConfig } from "@crust-seed/shared/configSchema";

const t = initTRPC.create();
const { router } = t;
const proc = t.procedure;

/** Never executed — the routers below are types, not an implementation. */
function unimplemented(): never {
	throw new Error("@crust-seed/api-types is a type-only contract");
}
const out = <T,>() => unimplemented() as T;

// ─── Shared payload shapes ──────────────────────────────────────────────────

export type IndexerStatus = "OK" | "RATE_LIMITED" | "UNKNOWN_ERROR";

export interface IndexerCategories {
	tv: boolean;
	movie: boolean;
	anime: boolean;
	xxx: boolean;
	audio: boolean;
	book: boolean;
	additional: boolean;
}

export interface IndexerLimits {
	default: number;
	max: number;
}

export interface IdSearchCaps {
	tvdbId?: boolean;
	tmdbId?: boolean;
	imdbId?: boolean;
	tvMazeId?: boolean;
}

export interface Indexer {
	id: number;
	name: string | null;
	url: string;
	apikey: string;
	trackers: string[] | null;
	enabled: boolean;
	status: IndexerStatus | null;
	retryAfter: number | null;
	searchCap: boolean;
	tvSearchCap: boolean;
	movieSearchCap: boolean;
	musicSearchCap: boolean;
	audioSearchCap: boolean;
	bookSearchCap: boolean;
	tvIdCaps: IdSearchCaps | null;
	movieIdCaps: IdSearchCaps | null;
	categories: IndexerCategories | null;
	limits: IndexerLimits | null;
}

export type ProblemSeverity = "error" | "warning" | "info";

export interface Problem {
	id: string;
	severity: ProblemSeverity;
	summary: string;
	details?: string;
	metadata?: Record<string, unknown>;
}

export interface DbDiagnostics {
	path: string;
	sizes: { db: number | null; wal: number | null; shm: number | null };
	pageSize: number | null;
	pageCount: number | null;
	freelistCount: number | null;
	freeBytes: number | null;
	freePercent: number | null;
	dbstatTop?: { name: string; bytes: number; pages: number }[];
	dbstatError?: string;
	error?: string;
}

export interface LogEntry {
	timestamp: string;
	level: string;
	label?: string;
	message: string;
}

export interface BuildInfo {
	commitSha: string | null;
	branch: string | null;
	tag: string | null;
	message: string | null;
	date: string | null;
}

export type JobName =
	| "rss"
	| "search"
	| "cleanup"
	| "inject"
	| "updateIndexerCaps";

export interface JobStatus {
	name: JobName;
	interval: string;
	lastExecution: string | null;
	lastDuration: string | null;
	nextExecution: string;
	isActive: boolean;
	canRunNow: boolean;
}

export interface SearcheeListItem {
	id: number | string;
	name: string;
	indexerCount: number;
	firstSearchedAt: string | null;
	lastSearchedAt: string | null;
	label: string | null;
	source: string | null;
	length: number | null;
	clientHost: string | null;
}

export interface NotificationResult {
	ok: boolean;
	url: string;
	status?: number;
	error?: string;
}

// ─── Input schemas (mirrored from the Rust handlers) ────────────────────────

const loginInputSchema = z.object({
	username: z.string().min(1, "Username is required"),
	password: z.string().min(1, "Password is required"),
});

const setupInputSchema = z.object({
	username: z.string().min(1, "Username is required"),
	password: z.string().min(8, "Password must be at least 8 characters"),
});

const webhookSchema = z.union([
	z.string(),
	z.object({
		url: z.string().url(),
		payload: z.record(z.string(), z.unknown()).optional(),
		headers: z.record(z.string(), z.string()).optional(),
	}),
]);

const indexerCreateSchema = z.object({
	name: z.string().min(1).optional(),
	url: z.string().url(),
	apikey: z.string().min(1),
	enabled: z.boolean().default(true),
});

const indexerUpdateSchema = z.object({
	id: z.number().int().positive(),
	name: z.string().min(1).optional().nullable(),
	url: z.string().url().optional(),
	apikey: z.string().min(1).optional(),
	enabled: z.boolean().optional(),
});

// ─── Routers ────────────────────────────────────────────────────────────────

const authRouter = router({
	authStatus: proc.query(() =>
		out<{
			userExists: boolean;
			signupAllowed: boolean;
			signupWindowMsRemaining: number;
			isDocker: boolean;
			isLoggedIn: boolean;
			user: { id: number; username: string } | null;
		}>(),
	),
	setup: proc.input(setupInputSchema).mutation(() => out<void>()),
	logIn: proc.input(loginInputSchema).mutation(() => out<void>()),
	logOut: proc.mutation(() => out<void>()),
});

const settingsRouter = router({
	get: proc.query(() => out<{ config: RuntimeConfig; apiKey: string }>()),
	save: proc
		.input(z.record(z.string(), z.unknown()))
		.mutation(() => out<{ success: true }>()),
	replace: proc
		.input(z.record(z.string(), z.unknown()))
		.mutation(() => out<{ success: true }>()),
	setApiKey: proc
		.input(z.object({ apiKey: z.string().min(24) }))
		.mutation(() => out<{ apiKey: string }>()),
	resetApiKey: proc.mutation(() => out<{ apiKey: string }>()),
	validate: proc.query(() =>
		out<{
			status: string;
			validations: { paths: boolean; torznab: boolean };
		}>(),
	),
	testNotification: proc
		.input(z.object({ webhooks: z.array(webhookSchema) }))
		.mutation(() => out<{ results: NotificationResult[] }>()),
});

const logsRouter = router({
	getVerbose: proc.query(() => out<string>()),
	getRecentLogs: proc
		.input(z.object({ limit: z.number().min(1).max(1000).default(100) }))
		.query(() => out<LogEntry[]>()),
	subscribe: proc
		.input(z.object({ limit: z.number().min(1).max(500).default(100) }))
		.subscription(() => out<AsyncIterable<LogEntry>>()),
});

const jobsRouter = router({
	getJobStatuses: proc.query(() => out<JobStatus[]>()),
	triggerJob: proc
		.input(
			z.object({
				name: z.enum([
					"rss",
					"search",
					"cleanup",
					"inject",
					"updateIndexerCaps",
				]),
			}),
		)
		.mutation(() => out<{ success: boolean; message: string }>()),
});

const statsRouter = router({
	getOverview: proc.query(() =>
		out<{
			totalSearchees: number;
			totalMatches: number;
			totalIndexers: number;
			healthyIndexers: number;
			recentMatches: number;
			matchRate: number;
			matchesPerSnatch: number;
			matchesPerQuery: number;
			matchesPerQueryIndexer: number;
			snatchCount: number;
			queryCount: number;
			queryIndexerCount: number;
			wastedSnatchCount: number;
			wastedSnatchRate: number;
			unhealthyIndexers: number;
			allIndexersHealthy: boolean;
			decisionBreakdown: { decision: string; count: number }[];
		}>(),
	),
	getIndexerStats: proc.query(() =>
		out<
			{
				id: number;
				name: string;
				enabled: boolean;
				status: string;
			}[]
		>(),
	),
});

const indexersRouter = router({
	getAll: proc.query(() => out<Indexer[]>()),
	mergeDisabled: proc
		.input(
			z.object({
				sourceId: z.number().int().positive(),
				targetId: z.number().int().positive(),
			}),
		)
		.mutation(() => out<{ mergedCount: number; deleted: boolean }>()),
	create: proc.input(indexerCreateSchema).mutation(() => out<Indexer>()),
	update: proc.input(indexerUpdateSchema).mutation(() => out<Indexer>()),
	delete: proc
		.input(z.object({ id: z.number().int().positive() }))
		.mutation(() => out<{ success: true; indexer: Indexer }>()),
	testExisting: proc
		.input(z.object({ id: z.number().int().positive() }))
		.mutation(() => out<{ success: true; message: string }>()),
	testNew: proc
		.input(z.object({ url: z.string().url(), apikey: z.string().min(1) }))
		.mutation(() => out<{ success: true; message: string }>()),
});

const healthRouter = router({
	get: proc.query(() =>
		out<{ problems: Problem[]; diagnostics: { db: DbDiagnostics } }>(),
	),
});

const searcheesRouter = router({
	list: proc
		.input(
			z
				.object({
					search: z.string().trim().min(1).max(200).optional(),
					limit: z.number().int().min(1).max(200).default(50),
					offset: z.number().int().min(0).default(0),
				})
				.default({ limit: 50, offset: 0 }),
		)
		.query(() =>
			out<{
				total: number;
				pagination: { limit: number; offset: number };
				indexerTotals: { configured: number; enabled: number };
				items: SearcheeListItem[];
			}>(),
		),
	bulkSearch: proc
		.input(
			z.object({
				names: z
					.array(z.string().trim().min(1).max(500))
					.min(1, "Select at least one item")
					.max(20, "You can only bulk search up to 20 items at a time"),
				force: z.boolean().optional().default(false),
			}),
		)
		.mutation(() =>
			out<{
				requested: number;
				attempted: number;
				totalFound: number;
				skipped: number;
			}>(),
		),
});

const clientsRouter = router({
	testConnection: proc
		.input(
			z.object({
				client: z.enum([
					"qbittorrent",
					"rtorrent",
					"transmission",
					"deluge",
				]),
				url: z.string().url(),
				username: z.string().optional(),
				password: z.string().optional(),
				readonly: z.boolean().default(false),
				plugin: z.boolean().default(false).optional(),
			}),
		)
		.mutation(() => out<{ success: boolean; message: string }>()),
});

const metaRouter = router({
	getBuildInfo: proc.query(() =>
		out<{ appName: string; version: string; build: BuildInfo }>(),
	),
});

export const appRouter = router({
	auth: authRouter,
	settings: settingsRouter,
	logs: logsRouter,
	jobs: jobsRouter,
	stats: statsRouter,
	indexers: indexersRouter,
	health: healthRouter,
	searchees: searcheesRouter,
	clients: clientsRouter,
	meta: metaRouter,
});

export type AppRouter = typeof appRouter;
