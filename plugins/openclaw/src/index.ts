import { execSync, spawn, ChildProcess } from "child_process";
import { existsSync } from "fs";
import { resolve } from "path";

let tapProcess: ChildProcess | null = null;

export default {
  id: "tapfs",
  name: "TapFS",

  register(api: any) {
    const config = api.pluginConfig;
    const pluginRoot = api.resolvePath(".");
    const launcherPath = resolve(pluginRoot, "..", "shared", "tapfs-launcher");

    api.registerService({
      name: "tapfs-mount",

      start: () => {
        const env = {
          ...process.env,
          TAPFS_PLUGIN_ROOT: pluginRoot,
          TAPFS_CONNECTOR: config.connector || "",
          TAPFS_TOKEN: config.apiToken || "",
          TAPFS_BASE_URL: config.baseUrl || "",
          TAPFS_MOUNT_POINT: config.mountPoint || "/tmp/tap",
        };

        tapProcess = spawn(launcherPath, ["start"], {
          env,
          stdio: "inherit",
          detached: false,
        });

        api.logger.info(`tapfs starting (PID ${tapProcess.pid})`);
      },

      stop: () => {
        if (tapProcess) {
          tapProcess.kill("SIGINT");
          tapProcess = null;
        }
        try {
          execSync(`${launcherPath} stop`, { stdio: "ignore" });
        } catch {
          // ignore cleanup errors
        }
        api.logger.info("tapfs stopped");
      },

      healthCheck: () => {
        const mountPoint = config.mountPoint || "/tmp/tap";
        return existsSync(resolve(mountPoint, "AGENTS.md"));
      },
    });
  },
};
