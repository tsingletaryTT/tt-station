import Foundation

/// Builds the pieces for a **box-hosted** Open WebUI: a docker command to run
/// (over SSH) ON the box, and the URLs the Mac opens/polls.
///
/// Open WebUI runs on the QuietBox, not on the Mac. This deletes every
/// Mac-side install failure mode (uv/Python-version/wheel builds, and an
/// ambient `DATABASE_URL` in the user's shell crashing startup): the box has
/// docker, sits right next to the vLLM `/v1` it talks to, and one container
/// serves any browser on the LAN. The Mac just SSHes the launch and opens a
/// browser tab. Pure/table-driven so the exact command + URLs are unit-tested.
public enum OpenWebUILauncher {
    /// The docker container name on the box — stable so relaunches reuse it.
    public static let containerName = "ttstation-openwebui"
    /// Host port published ON THE BOX. Not 8080: that's already taken on the
    /// QuietBox (something else is bound there), so we publish on 3000.
    public static let boxPort = 8080  // container's internal port
    public static let hostPort = 3000  // published host port on the box

    /// An idempotent shell script to (re)launch Open WebUI as a docker
    /// container on the box, wired to the box's LOCAL vLLM on `servingPort`.
    /// Meant to be run over SSH on the box (`ttuser@<box>`).
    ///
    /// Behavior: if the container is already running, do nothing (fast reuse);
    /// otherwise remove any stale one and `docker run -d` a fresh one. The
    /// container reaches the host's vLLM via `host.docker.internal` (mapped to
    /// the host gateway, the portable way for a bridge-network container to
    /// hit a port on its Linux host). A named volume persists chats/settings
    /// across relaunches. `WEBUI_AUTH=false` skips the account wall for a
    /// quick demo; `OPENAI_API_KEY` is a throwaway (the box `/v1` needs none).
    ///
    /// The Open WebUI image ref pulled/run on the box.
    public static let image = "ghcr.io/open-webui/open-webui:main"

    /// `servingPort` is an `Int`, so there is no injection surface in the
    /// interpolation below.
    ///
    /// The image is pre-pulled in a small retry loop before `docker run`: on a
    /// fresh box the first pull was observed to fail intermittently with an
    /// IPv6 `connection reset by peer` from ghcr.io, and a bare `docker run`
    /// would surface that transient failure to the user. Retrying the pull
    /// (docker caches completed layers, so a retry resumes) makes the first
    /// launch resilient; once the image is local the loop is a fast no-op.
    public static func dockerCommand(servingPort: Int) -> String {
        """
        if [ "$(docker inspect -f '{{.State.Running}}' \(containerName) 2>/dev/null)" = "true" ]; then exit 0; fi
        docker rm -f \(containerName) >/dev/null 2>&1 || true
        if ! docker image inspect \(image) >/dev/null 2>&1; then
          for i in 1 2 3 4 5; do docker pull \(image) && break; sleep 3; done
        fi
        docker run -d --name \(containerName) \
          -p \(hostPort):\(boxPort) \
          --add-host=host.docker.internal:host-gateway \
          -e OPENAI_API_BASE_URL=http://host.docker.internal:\(servingPort)/v1 \
          -e OPENAI_API_KEY=sk-none -e WEBUI_AUTH=false \
          -v \(containerName):/app/backend/data \
          \(image)
        """
    }

    /// The browser URL to open once Open WebUI is up on the box.
    public static func url(host: String) -> URL {
        URL(string: "http://\(host):\(hostPort)")!
    }

    /// The health endpoint to poll while waiting for the box container to come
    /// up (first run pulls the image + initializes).
    public static func healthURL(host: String) -> URL {
        URL(string: "http://\(host):\(hostPort)/health")!
    }
}
