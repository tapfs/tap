/**
 * E2B sandbox template with tapfs pre-installed.
 *
 * Build: npx e2b template build
 * Use:   const sandbox = await Sandbox.create("tapfs")
 */
import { Template } from "@e2b/sdk";

const template = Template()
  .fromUbuntuImage("24.04")
  .aptInstall(["fuse", "libfuse-dev", "curl"])
  .runCmd([
    "curl -fsSL https://github.com/tapfs/tap/releases/latest/download/tap-linux-x64 -o /usr/local/bin/tap",
    "chmod +x /usr/local/bin/tap",
  ])
  .setEnvs({
    TAPFS_MOUNT_POINT: "/tmp/tap",
  });

export default template;
