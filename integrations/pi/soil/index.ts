// SPDX-License-Identifier: Apache-2.0 OR MIT
/**
 * Soil for Pi: structural reads/edits, load checks, eval and git checkpoints.
 *
 * The file on disk stays canonical. This extension builds a small form outline,
 * applies top-level structural edits to a candidate, validates the candidate
 * with `soil-lang --no-call`, then writes atomically. A failed post-apply check
 * restores the original bytes.
 *
 * Deliberate v1 limits:
 *  - structural edits address top-level forms (form index) only;
 *  - check is parse/load, not a type checker;
 *  - eval is explicit and may have side effects.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { withFileMutationQueue } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

type SForm = {
	start: number;
	end: number;
	kind: "list" | "atom" | "string";
	text: string;
	path: string;
	children: SForm[];
};

type CheckResult = {
	ok: boolean;
	output: string;
	candidate?: string;
	tempFile?: string;
};

const CHECK_TIMEOUT_MS = 30_000;
const TEST_TIMEOUT_MS = 180_000;
const EVAL_TIMEOUT_MS = 30_000;

/** Parse enough S-expressions to make structural edits and an outline. */
function parseSoil(source: string): { forms: SForm[]; error?: string } {
	let pos = 0;
	let error: string | undefined;
	const length = source.length;

	function skipTrivia() {
		while (pos < length) {
			const ch = source[pos];
			if (ch === ";" ) {
				while (pos < length && source[pos] !== "\n") pos++;
				continue;
			}
			if (/\s/.test(ch)) {
				pos++;
				continue;
			}
			break;
		}
	}

	function parseForm(path: string): SForm | undefined {
		skipTrivia();
		if (pos >= length) return undefined;
		const start = pos;
		const ch = source[pos];

		if (ch === "(") {
			pos++;
			const children: SForm[] = [];
			for (;;) {
				skipTrivia();
				if (pos >= length) {
					error = `unterminated list at byte ${start}`;
					break;
				}
				if (source[pos] === ")") {
					pos++;
					break;
				}
				const child = parseForm(`${path}.${children.length}`);
				if (!child) {
					error = `could not parse form at byte ${pos}`;
					break;
				}
				children.push(child);
			}
			return {
				start,
				end: pos,
				kind: "list",
				text: source.slice(start, pos),
				path,
				children,
			};
		}

		if (ch === '"') {
			pos++;
			let escaped = false;
			while (pos < length) {
				const next = source[pos++];
				if (escaped) {
					escaped = false;
					continue;
				}
				if (next === "\\") {
					escaped = true;
					continue;
				}
				if (next === '"') break;
			}
			return {
				start,
				end: pos,
				kind: "string",
				text: source.slice(start, pos),
				path,
				children: [],
			};
		}

		if (ch === "'") {
			pos++;
			const child = parseForm(path);
			const end = child?.end ?? pos;
			return {
				start,
				end,
				kind: "atom",
				text: source.slice(start, end),
				path,
				children: child ? [child] : [],
			};
		}

		while (pos < length) {
			const next = source[pos];
			if (/\s/.test(next) || next === "(" || next === ")" || next === ";") break;
			pos++;
		}
		if (pos === start) pos++; // stray byte; do not loop forever
		return {
			start,
			end: pos,
			kind: "atom",
			text: source.slice(start, pos),
			path,
			children: [],
		};
	}

	const forms: SForm[] = [];
	while (true) {
		skipTrivia();
		if (pos >= length) break;
		const form = parseForm(String(forms.length));
		if (!form) break;
		forms.push(form);
		if (error) break;
	}
	return { forms, error };
}

function outline(form: SForm, depth = 0): unknown {
	const head =
		form.kind === "list" && form.children.length > 0 ? form.children[0].text : undefined;
	return {
		path: form.path,
		kind: form.kind,
		head,
		start: form.start,
		end: form.end,
		children: form.children.length,
		preview: form.text.replace(/\s+/g, " ").slice(0, 160),
		nested: depth < 2 ? form.children.map((child) => outline(child, depth + 1)) : undefined,
	};
}

async function git(pi: ExtensionAPI, root: string, args: string[], timeout = 30_000) {
	return pi.exec("git", args, { cwd: root, timeout });
}

async function repoRoot(pi: ExtensionAPI, start: string): Promise<string> {
	if (process.env.SOIL_ROOT) return resolve(process.env.SOIL_ROOT);
	const isSoil = (path: string) =>
		existsSync(join(path, "crates/soil-lang")) || existsSync(join(path, "soil/crates/soil-lang"));
	const result = await git(pi, start, ["rev-parse", "--show-toplevel"]);
	if (result.code === 0 && result.stdout.trim()) {
		const root = result.stdout.trim();
		if (isSoil(root)) return root;
	}
	if (isSoil(start)) return start;
	let current = start;
	for (;;) {
		if (isSoil(current)) return current;
		const parent = dirname(current);
		if (parent === current) break;
		current = parent;
	}
	throw new Error("could not find a Soil checkout (set SOIL_ROOT or run inside soil)");
}

function soilCommand(root: string): { command: string; prefix: string[] } {
	const explicit = process.env.SOIL_LANG_BIN;
	if (explicit) return { command: explicit, prefix: [] };
	for (const candidate of ["target/debug/soil-lang", "target/release/soil-lang"]) {
		const path = join(root, candidate);
		if (existsSync(path)) return { command: path, prefix: [] };
	}
	return {
		command: "cargo",
		prefix: ["run", "-q", "-p", "soil-lang", "--bin", "soil-lang", "--"],
	};
}

function prelude(root: string): string {
	return join(root, "crates/soil-lang/stdlib/prelude.soil");
}

async function checkSource(
	pi: ExtensionAPI,
	root: string,
	source: string | undefined,
	file: string,
): Promise<CheckResult> {
	const temp = await mkdtemp(join(tmpdir(), "pi-soil-check-"));
	const records = join(temp, "records");
	await mkdir(records, { recursive: true });
	const candidate = source === undefined ? file : join(temp, basename(file) || "candidate.soil");
	if (source !== undefined) await writeFile(candidate, source, "utf8");
	const { command, prefix } = soilCommand(root);
	const args = [
		...prefix,
		"--prelude",
		prelude(root),
		"--records-root",
		records,
		"--no-call",
		candidate,
	];
	const result = await pi.exec(command, args, { cwd: root, timeout: CHECK_TIMEOUT_MS });
	const output = [result.stdout, result.stderr].filter(Boolean).join("\n").trim();
	return { ok: result.code === 0, output, candidate, tempFile: temp };
}

async function atomicWrite(file: string, text: string) {
	const dir = dirname(file);
	const temp = join(dir, `.${basename(file)}.pi-soil-${process.pid}.tmp`);
	await writeFile(temp, text, "utf8");
	await rename(temp, file);
}

function textContent(text: string) {
	return [{ type: "text" as const, text }];
}

function candidateEdit(
	source: string,
	forms: SForm[],
	op: string,
	formIndex: number | undefined,
	content: string,
): string {
	if (op === "append") {
		return `${source.trimEnd()}\n\n${content.trim()}\n`;
	}
	if (formIndex === undefined || !Number.isInteger(formIndex) || formIndex < 0 || formIndex >= forms.length) {
		throw new Error(`form must be an integer in 0..${forms.length - 1}`);
	}
	const form = forms[formIndex];
	if (op === "replace") return source.slice(0, form.start) + content.trimEnd() + source.slice(form.end);
	if (op === "delete") return source.slice(0, form.start) + source.slice(form.end).replace(/^\s*\n?/, "\n");
	if (op === "insert_before") return source.slice(0, form.start) + content.trimEnd() + "\n\n" + source.slice(form.start);
	if (op === "insert_after") return source.slice(0, form.end) + "\n\n" + content.trimEnd() + source.slice(form.end);
	throw new Error(`unknown op ${op}`);
}

async function checkpoint(
	pi: ExtensionAPI,
	root: string,
	message: string | undefined,
	paths: string[] | undefined,
): Promise<{ ok: boolean; output: string }> {
	const branch = await git(pi, root, ["rev-parse", "--abbrev-ref", "HEAD"]);
	const name = branch.stdout.trim();
	if (name === "main" || name === "master") {
		return {
			ok: false,
			output: `refusing to checkpoint on ${name}; create a feature branch first`,
		};
	}
	let files = paths?.filter(Boolean) ?? [];
	if (files.length === 0) {
		const tracked = await git(pi, root, ["diff", "--name-only", "--", "*.soil"]);
		const untracked = await git(pi, root, ["ls-files", "--others", "--exclude-standard", "--", "*.soil"]);
		files = [...tracked.stdout.split("\n"), ...untracked.stdout.split("\n")].map((s) => s.trim()).filter(Boolean);
	}
	files = [...new Set(files)];
	if (files.length === 0) return { ok: true, output: "no changed .soil files" };
	const add = await git(pi, root, ["add", "--", ...files]);
	if (add.code !== 0) return { ok: false, output: add.stderr || add.stdout };
	const msg = message?.trim() || "soil: checkpoint";
	const commit = await git(pi, root, ["commit", "-m", msg, "--", ...files]);
	return {
		ok: commit.code === 0,
		output: [commit.stdout, commit.stderr].filter(Boolean).join("\n").trim(),
	};
}

function runWithInput(
	command: string,
	args: string[],
	cwd: string,
	input: string,
	timeoutMs: number,
): Promise<{ code: number; stdout: string; stderr: string }> {
	return new Promise((resolvePromise, reject) => {
		const child = spawn(command, args, { cwd, stdio: ["pipe", "pipe", "pipe"] });
		let stdout = "";
		let stderr = "";
		let settled = false;
		const timer = setTimeout(() => {
			if (settled) return;
			settled = true;
			child.kill("SIGKILL");
			reject(new Error(`timed out after ${timeoutMs} ms`));
		}, timeoutMs);
		child.stdout.on("data", (chunk) => (stdout += chunk.toString()));
		child.stderr.on("data", (chunk) => (stderr += chunk.toString()));
		child.on("error", (error) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			reject(error);
		});
		child.on("close", (code) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			resolvePromise({ code: code ?? 1, stdout, stderr });
		});
		child.stdin.write(input);
		child.stdin.end();
	});
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "soil_read",
		label: "Soil read",
		description: "Read a .soil file as a form outline with paths, spans and previews.",
		parameters: Type.Object({
			file: Type.String({ description: "Path to a .soil file" }),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const file = isAbsolute(params.file) ? params.file : join(ctx.cwd, params.file);
			const root = await repoRoot(pi, dirname(file));
			const source = await readFile(file, "utf8");
			const parsed = parseSoil(source);
			const text = parsed.error
				? `parse error: ${parsed.error}`
				: JSON.stringify(
						{
							file,
							forms: parsed.forms.length,
							outline: parsed.forms.map((form) => outline(form)),
						},
						null,
						2,
					);
			return {
				content: textContent(text),
				details: { file, forms: parsed.forms.length, error: parsed.error, root },
			};
		},
	});

	pi.registerTool({
		name: "soil_edit",
		label: "Soil structural edit",
		description:
			"Edit a top-level form in a .soil file. The candidate is parsed/loaded with soil-lang before it is written; a failed post-apply check restores the original. Use form 0 for the first top-level form. Set dry_run to preview without writing.",
		parameters: Type.Object({
			file: Type.String({ description: "Path to a .soil file" }),
			op: Type.Union([
				Type.Literal("replace"),
				Type.Literal("insert_before"),
				Type.Literal("insert_after"),
				Type.Literal("delete"),
				Type.Literal("append"),
			]),
			form: Type.Optional(Type.Number({ description: "Zero-based top-level form index (not needed for append)" })),
			content: Type.Optional(Type.String({ description: "Soil source for replace/insert/append" })),
			dry_run: Type.Optional(Type.Boolean({ description: "Validate and report without writing" })),
			check: Type.Optional(Type.Boolean({ description: "Load-check the candidate before apply (default true)" })),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const file = isAbsolute(params.file) ? params.file : join(ctx.cwd, params.file);
			const root = await repoRoot(pi, dirname(file));
			const source = await readFile(file, "utf8");
			const parsed = parseSoil(source);
			if (parsed.error) {
				return {
					content: textContent(`parse error: ${parsed.error}`),
					details: { ok: false, file, error: parsed.error },
				};
			}
			const content = params.content ?? "";
			if (params.op !== "delete" && params.op !== "append" && !content.trim()) {
				throw new Error(`${params.op} needs content`);
			}
			const candidate = candidateEdit(source, parsed.forms, params.op, params.form, content);
			const before = params.form !== undefined ? parsed.forms[params.form]?.text : undefined;
			const shouldCheck = params.check !== false;
			let check: CheckResult | undefined;
			if (shouldCheck) {
				check = await checkSource(pi, root, candidate, file);
				if (!check.ok) {
					await rm(check.tempFile!, { recursive: true, force: true });
					return {
						content: textContent(`candidate rejected by soil-lang:\n${check.output}`),
						details: { ok: false, file, before, check: check.output },
					};
				}
			}

			if (params.dry_run) {
				if (check?.tempFile) await rm(check.tempFile, { recursive: true, force: true });
				return {
					content: textContent(
						`dry run: ${params.op} ${params.form ?? "appended"}\n\n--- before\n${before ?? ""}\n\n--- candidate\n${candidate.slice(
							Math.max(0, (before ? source.indexOf(before) : 0) - 80),
							Math.min(candidate.length, (before ? source.indexOf(before) : 0) + candidate.length + 80),
						)}`,
					),
					details: { ok: true, dryRun: true, file, before },
				};
			}

			let restored = false;
			await withFileMutationQueue(file, async () => {
				await atomicWrite(file, candidate);
				if (shouldCheck) {
					const after = await checkSource(pi, root, undefined, file);
					if (!after.ok) {
						await atomicWrite(file, source);
						restored = true;
						check = after;
					}
				}
			});
			if (check?.tempFile) await rm(check.tempFile, { recursive: true, force: true }).catch(() => {});
			if (restored) {
				return {
					content: textContent(`apply failed; original restored\n${check?.output ?? ""}`),
					details: { ok: false, file, restored: true, check: check?.output },
				};
			}
			return {
				content: textContent(
					`applied ${params.op}${params.form !== undefined ? ` form ${params.form}` : ""} to ${file}`,
				),
				details: { ok: true, file, before, check: check?.output },
			};
		},
	});

	pi.registerTool({
		name: "soil_check",
		label: "Soil check",
		description:
			"Parse and load a .soil file with soil-lang. Optionally run the Soil test corpus. This does not evaluate mutating forms unless the file itself does so at top level.",
		parameters: Type.Object({
			file: Type.String({ description: "Path to a .soil file" }),
			tests: Type.Optional(Type.Boolean({ description: "Run cargo test -p soil-lang --test soil_lisp" })),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const file = isAbsolute(params.file) ? params.file : join(ctx.cwd, params.file);
			const root = await repoRoot(pi, dirname(file));
			const check = await checkSource(pi, root, undefined, file);
			await rm(check.tempFile!, { recursive: true, force: true }).catch(() => {});
			let testOutput = "";
			let testsOk = true;
			if (params.tests) {
				const result = await pi.exec(
					"cargo",
					["test", "-p", "soil-lang", "--test", "soil_lisp"],
					{ cwd: root, timeout: TEST_TIMEOUT_MS },
				);
				testsOk = result.code === 0;
				testOutput = [result.stdout, result.stderr].filter(Boolean).join("\n").trim();
			}
			const ok = check.ok && testsOk;
			return {
				content: textContent(
					[check.ok ? "load: ok" : "load: failed", check.output, params.tests ? (testsOk ? "tests: ok" : "tests: failed") : "", testOutput]
						.filter(Boolean)
						.join("\n\n"),
				),
				details: { ok, check: check.output, tests: params.tests ? testsOk : undefined },
			};
		},
	});

	pi.registerTool({
		name: "soil_eval",
		label: "Soil eval",
		description:
			"Evaluate a Soil expression in the context of a file and print the result. Evaluation can have side effects; use a plan/dry-run function when one exists.",
		parameters: Type.Object({
			expr: Type.String({ description: "Soil expression to evaluate" }),
			file: Type.Optional(Type.String({ description: "Optional .soil file to load first" })),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const file = params.file
				? isAbsolute(params.file)
					? params.file
					: join(ctx.cwd, params.file)
				: undefined;
			const root = await repoRoot(pi, file ? dirname(file) : ctx.cwd);
			const temp = await mkdtemp(join(tmpdir(), "pi-soil-eval-"));
			const records = join(temp, "records");
			await mkdir(records, { recursive: true });
			const { command, prefix } = soilCommand(root);
			const args = [
				...prefix,
				"--prelude",
				prelude(root),
				"--records-root",
				records,
				...(file ? [file] : []),
				"--repl",
			];
			const result = await runWithInput(
				command,
				args,
				root,
				`(println ${params.expr})\n`,
				EVAL_TIMEOUT_MS,
			);
			await rm(temp, { recursive: true, force: true }).catch(() => {});
			const output = result.stdout
				.split("\n")
				.filter((line) => line.trim() && line.trim() !== ">" && line.trim() !== "...")
				.join("\n")
				.trim();
			return {
				content: textContent(output || result.stderr || "(no output)"),
				details: { ok: result.code === 0, expr: params.expr, stderr: result.stderr },
			};
		},
	});

	pi.registerTool({
		name: "soil_diff",
		label: "Soil diff",
		description: "Show the git diff for a Soil file or the repo's .soil changes.",
		parameters: Type.Object({
			file: Type.Optional(Type.String({ description: "Optional .soil file" })),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const file = params.file
				? isAbsolute(params.file)
					? params.file
					: join(ctx.cwd, params.file)
				: undefined;
			const root = await repoRoot(pi, file ? dirname(file) : ctx.cwd);
			const args = ["diff", "--"].concat(file ? [file] : ["*.soil"]);
			const result = await git(pi, root, args);
			return {
				content: textContent(result.stdout || result.stderr || "(no diff)"),
				details: { ok: result.code === 0, file: params.file },
			};
		},
	});

	pi.registerTool({
		name: "soil_checkpoint",
		label: "Soil checkpoint",
		description:
			"Commit changed .soil files on the current feature branch. Refuses main/master. Pass paths to restrict the transaction; otherwise all modified/untracked .soil files are included.",
		parameters: Type.Object({
			message: Type.Optional(Type.String({ description: "Commit message" })),
			paths: Type.Optional(Type.Array(Type.String({ description: "Restrict to these paths" }))),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const root = await repoRoot(pi, ctx.cwd);
			const result = await checkpoint(pi, root, params.message, params.paths);
			return {
				content: textContent(result.output || (result.ok ? "checkpointed" : "checkpoint failed")),
				details: { ok: result.ok, root },
			};
		},
	});

	pi.registerCommand("soil-checkpoint", {
		description: "Commit changed .soil files on the current feature branch",
		handler: async (args, ctx) => {
			const root = await repoRoot(pi, ctx.cwd);
			const result = await checkpoint(pi, root, args?.trim() || undefined, undefined);
			if (ctx.hasUI) ctx.ui.notify(result.output, result.ok ? "info" : "error");
		},
	});

	pi.registerCommand("soil-check", {
		description: "Parse/load a .soil file: /soil-check path/to/file.soil",
		handler: async (args, ctx) => {
			const file = args?.trim();
			if (!file) {
				if (ctx.hasUI) ctx.ui.notify("usage: /soil-check path/to/file.soil", "warning");
				return;
			}
			const path = isAbsolute(file) ? file : join(ctx.cwd, file);
			const root = await repoRoot(pi, dirname(path));
			const check = await checkSource(pi, root, undefined, path);
			await rm(check.tempFile!, { recursive: true, force: true }).catch(() => {});
			if (ctx.hasUI) ctx.ui.notify(check.ok ? "load: ok" : `load failed:\n${check.output}`, check.ok ? "info" : "error");
		},
	});

	pi.on("tool_call", async (event, ctx) => {
		if (event.toolName !== "edit" && event.toolName !== "write") return undefined;
		const path = String((event.input as Record<string, unknown>)?.path ?? "");
		if (!path.endsWith(".soil")) return undefined;
		if (process.env.SOIL_PI_ALLOW_RAW_EDITS === "1") return undefined;
		if (ctx.hasUI) ctx.ui.notify(`blocked raw .soil edit: ${path}`, "warning");
		return {
			block: true,
			reason:
				"Raw .soil edits are disabled. Use soil_edit, or set SOIL_PI_ALLOW_RAW_EDITS=1 for an explicit bypass.",
		};
	});
}
