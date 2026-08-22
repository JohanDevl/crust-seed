import { Generator, getConfig } from "@tanstack/router-generator";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const webuiRoot = join(repoRoot, "webui");

const config = getConfig(
	{
		target: "react",
		autoCodeSplitting: true,
	},
	webuiRoot,
);

await new Generator({ config, root: webuiRoot }).run();
