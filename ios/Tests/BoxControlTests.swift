import XCTest
@testable import TTStationMobile

/// The phone's box controls: versions, waking, and the shapes the agent sends
/// (`BoxControlModels.swift`, `BoxControlLogic.swift`).
final class ClientCompatTests: XCTestCase {
    private func status(_ extra: String) throws -> BoxStatus {
        let json = #"{"name":"qb2-lab","chips":"4xBH","status":"serving:Qwen/Qwen3-32B","device_mesh":"p300x2""# + extra + "}"
        return try JSONDecoder().decode(BoxStatus.self, from: Data(json.utf8))
    }

    func testAnOlderAgentWithoutVersionsIsFine() throws {
        let s = try status("")
        XCTAssertNil(s.apiVersion)
        XCTAssertEqual(ClientCompat.check(s), .ok)
        XCTAssertEqual(s.servingModel, "Qwen/Qwen3-32B")
    }

    func testTheBoxSaysWhenThisAppIsTooOld() throws {
        let s = try status(#","agent_version":"0.12.0","api_version":3,"min_client":2"#)
        guard case .updateApp(let message) = ClientCompat.check(s) else { return XCTFail("\(ClientCompat.check(s))") }
        XCTAssertTrue(message.contains("qb2-lab needs a newer TT Station app"), message)
    }

    func testTheSameGenerationIsFine() throws {
        let s = try status(#","agent_version":"0.11.0","api_version":1,"min_client":1,"mac":"aa:bb:cc:dd:ee:ff","model_health":{"state":"degraded","reason":"no answer in 60 s"}"#)
        XCTAssertEqual(ClientCompat.check(s), .ok)
        XCTAssertEqual(s.modelHealth?.isDegraded, true)
        XCTAssertEqual(s.modelHealth?.reason, "no answer in 60 s")
    }

    func testAnOwnerOnlyRefusalReadsPlainly() {
        let model = AgentError.http(403, "Only owners can start or stop models on this box.")
        XCTAssertEqual(model.errorDescription, "Only owners can start or stop models on this box.")
        XCTAssertTrue(model.isOwnerOnly)
        XCTAssertEqual(AgentError.http(403, "").errorDescription, "Only an owner of this box can do that.")
        XCTAssertEqual(AgentError.http(403, "Forbidden").errorDescription, "Only an owner of this box can do that.")
        XCTAssertFalse(AgentError.http(409, "x").isOwnerOnly)
        XCTAssertTrue(AgentError.unreachable(address: "a", reason: "b").isUnreachable)
    }
}

final class WakeTests: XCTestCase {
    func testAMagicPacketIsSixFFsAndTheMACSixteenTimes() throws {
        let data = try XCTUnwrap(MagicPacket.make(mac: "9C:6B:00:AB:4B:83"))
        XCTAssertEqual(data.count, 102)
        XCTAssertEqual(Array(data.prefix(6)), [UInt8](repeating: 0xFF, count: 6))
        XCTAssertEqual(Array(data[6..<12]), [0x9C, 0x6B, 0x00, 0xAB, 0x4B, 0x83])
        XCTAssertEqual(Array(data.suffix(6)), [0x9C, 0x6B, 0x00, 0xAB, 0x4B, 0x83])
        XCTAssertNotNil(MagicPacket.make(mac: "9c-6b-00-ab-4b-83"))
        XCTAssertNil(MagicPacket.make(mac: "9c:6b:00:ab:4b"))
        XCTAssertNil(MagicPacket.make(mac: "00:00:00:00:00:00"))
        XCTAssertNil(MagicPacket.make(mac: "zz:6b:00:ab:4b:83"))
    }

    /// This QuietBox as found: a wired port, unplugged, Wake-on-LAN off.
    private let asFound = #"""
    {"can_power_off":"yes","can_reboot":"yes","can_suspend":"yes","headless_power":false,"starts_at_boot":false,
     "wired":[{"name":"enp10s0","mac":"9c:6b:00:ab:4b:83","connected":false,"wake_enabled":false,"wake_supported":true}],
     "wake_on_lan":{"possible":false,"nic":"enp10s0","mac":"9c:6b:00:ab:4b:83","supported":true,"enabled":false,
       "persistent":false,"connected":false,"connection":"netplan-en","broadcast":["10.0.0.255"],
       "why_not":"Plug a network cable into enp10s0 and turn on Wake-on-LAN: Wi-Fi can't wake a sleeping or powered-off computer.",
       "enable_command":"sudo nmcli connection modify netplan-en 802-3-ethernet.wake-on-lan magic"},
     "setup":[]}
    """#

    func testThePhoneRemembersTheWiredPortNotStatusMAC() throws {
        let info = try JSONDecoder().decode(PowerInfo.self, from: Data(asFound.utf8))
        XCTAssertFalse(info.canWake)
        let target = try XCTUnwrap(WakeTarget(info))
        XCTAssertEqual(target.mac, "9c:6b:00:ab:4b:83")
        XCTAssertEqual(target.broadcast, ["10.0.0.255"])
        XCTAssertFalse(target.possible)
        XCTAssertTrue(target.whyNot!.contains("Plug a network cable"))
    }

    func testAnOlderAgentStillGivesAMAC() throws {
        let old = #"{"can_power_off":"yes","can_reboot":"yes","can_suspend":"yes","starts_at_boot":true,"wired":[{"name":"eno1","mac":"aa:bb:cc:dd:ee:01","connected":true,"wake_enabled":true}],"setup":[]}"#
        let info = try JSONDecoder().decode(PowerInfo.self, from: Data(old.utf8))
        XCTAssertNil(info.wakeOnLan)
        XCTAssertTrue(info.canWake)
        let target = try XCTUnwrap(WakeTarget(info))
        XCTAssertTrue(target.possible)
        XCTAssertEqual(target.mac, "aa:bb:cc:dd:ee:01")
        // Wi-Fi only: nothing to remember.
        let wifi = #"{"can_power_off":"yes","can_reboot":"yes","can_suspend":"yes","starts_at_boot":true,"wired":[],"setup":[]}"#
        XCTAssertNil(WakeTarget(try JSONDecoder().decode(PowerInfo.self, from: Data(wifi.utf8))))
    }

    func testBroadcastAddresses() {
        XCTAssertEqual(WakePlan.broadcast(ip: "10.0.0.122", netmask: "255.255.255.0"), "10.0.0.255")
        XCTAssertEqual(WakePlan.broadcast(ip: "192.168.1.20", netmask: "255.255.254.0"), "192.168.1.255")
        XCTAssertEqual(WakePlan.broadcast(ip: "172.16.5.4", netmask: "255.240.0.0"), "172.31.255.255")
        XCTAssertNil(WakePlan.broadcast(ip: "fe80::1", netmask: "255.255.255.0"))
    }

    func testWakingSaysPlainlyWhyNot() {
        let ready = WakeTarget(mac: "9c:6b:00:ab:4b:83", broadcast: ["10.0.0.255"], possible: true)

        guard case .cannot(let unknown) = WakePlan.plan(target: nil, host: "10.0.0.122", phoneBroadcast: "10.0.0.255")
        else { return XCTFail() }
        XCTAssertTrue(unknown.contains("hasn't learned how to wake"))

        let wifiOnly = WakeTarget(mac: "9c:6b:00:ab:4b:83", possible: false,
                                  whyNot: "This box has no wired network port, and Wi-Fi can't wake a sleeping or powered-off computer.")
        guard case .cannot(let why) = WakePlan.plan(target: wifiOnly, host: "10.0.0.122", phoneBroadcast: "10.0.0.255")
        else { return XCTFail() }
        XCTAssertTrue(why.contains("Wi-Fi can't wake"))
        XCTAssertTrue(why.contains("put just the model to sleep"), "offers model sleep instead: \(why)")

        guard case .cannot(let cellular) = WakePlan.plan(target: ready, host: "10.0.0.122", phoneBroadcast: nil)
        else { return XCTFail() }
        XCTAssertTrue(cellular.contains("same Wi-Fi"))

        guard case .cannot(let elsewhere) = WakePlan.plan(target: ready, host: "10.0.0.122", phoneBroadcast: "192.168.1.255")
        else { return XCTFail() }
        XCTAssertTrue(elsewhere.contains("isn't on the box's network"))
    }

    func testWakingSendsToTheLANThenEverywhereElseItMightReach() {
        let ready = WakeTarget(mac: "9c:6b:00:ab:4b:83", broadcast: ["10.0.0.255"], possible: true)
        XCTAssertEqual(WakePlan.plan(target: ready, host: "10.0.0.122", phoneBroadcast: "10.0.0.255"),
                       .send(["10.0.0.255", "255.255.255.255", "10.0.0.122"]))
        // A box known by name: no unicast fallback; a box that didn't list its networks: trust the phone's.
        let unlisted = WakeTarget(mac: "9c:6b:00:ab:4b:83", possible: true)
        XCTAssertEqual(WakePlan.plan(target: unlisted, host: "qb2.local", phoneBroadcast: "10.0.0.255"),
                       .send(["10.0.0.255", "255.255.255.255"]))
    }

    func testWhatThePhoneRemembersSurvivesARestart() throws {
        let target = WakeTarget(mac: "9c:6b:00:ab:4b:83", broadcast: ["10.0.0.255"], possible: true, whyNot: nil)
        let back = try JSONDecoder().decode(WakeTarget.self, from: JSONEncoder().encode(target))
        XCTAssertEqual(back, target)
    }
}

final class BoxControlShapesTests: XCTestCase {
    func testIdleSleepRoundTrips() throws {
        let json = #"{"settings":{"enabled":true,"after_minutes":60,"wake_on_request":true},"asleep":{"target":"Qwen/Qwen3-32B","community":false,"label":"Qwen3-32B","at":1760500000},"idle_for_s":null,"waking":false}"#
        let idle = try JSONDecoder().decode(IdleInfo.self, from: Data(json.utf8))
        XCTAssertEqual(idle.asleep?.label, "Qwen3-32B")
        XCTAssertEqual(idle.settings.afterMinutes, 60)
        let body = try JSONSerialization.jsonObject(with: JSONEncoder().encode(idle.settings)) as? [String: Any]
        XCTAssertEqual(body?["after_minutes"] as? Int, 60)
        XCTAssertEqual(body?["wake_on_request"] as? Bool, true)
    }

    func testDevicesAndTheActivityLog() throws {
        let devices = #"{"devices":[{"id":"d1","name":"Jane's iPhone","platform":"iOS","paired":1760000000,"last_used":1760500000,"expires":1770000000,"this_device":true,"owner":true,"person":null}],"owner":true,"idle_expiry_days":90}"#
        let list = try JSONDecoder().decode(DeviceList.self, from: Data(devices.utf8))
        XCTAssertTrue(list.owner)
        XCTAssertEqual(list.devices.first?.thisDevice, true)
        let audit = #"{"entries":[{"ts":1760500000,"device_id":"d1","device_name":"Jane's iPhone","action":"model_sleep","target":"Qwen/Qwen3-32B","result":"ok"}],"more":false,"offset":0}"#
        let page = try JSONDecoder().decode(AuditPage.self, from: Data(audit.utf8))
        XCTAssertEqual(page.entries.first?.action, "model_sleep")
        let settings = #"{"settings":{"keep_audio":false,"transcripts_to_library":true,"retention_days":null,"restart_last_model":false,"model_control":"owners"},"owner":false,"may_control_models":false}"#
        let info = try JSONDecoder().decode(DataSettingsInfo.self, from: Data(settings.utf8))
        let access = BoxAccess(owner: info.owner, mayControlModels: info.mayControlModels,
                               modelControl: info.settings.modelControl)
        XCTAssertEqual(access.modelsNote, "Only owners can start or stop models on this box.")
        XCTAssertNil(BoxAccess(owner: true, mayControlModels: true, modelControl: "owners").modelsNote)
    }

    /// `GET /code/models`, trimmed: the catalog model in use, a community package
    /// with two profiles, and one that can't run here.
    private let options = #"""
    {"code_model":"Qwen/Qwen3-32B","serve":{"phase":"serving","model":"Qwen/Qwen3-32B","since":1760500000,"error":null,"eta_s":null},
     "catalog_error":null,
     "models":[
      {"key":"community:acme/gpt-oss-20b#p150x2","group":"gpt-oss-20b","name":"gpt-oss-20b","model":"openai/gpt-oss-20b","source":"community","origin":"acme","profile":"p150x2","profile_label":"Interactive","profile_note":null,"chips":2,"context":131072,"tools":{"verified":true,"how":"tool parser openai"},"choice":{"action":"switch"},"serve":{"route":"/community/serve","body":{"id":"acme/gpt-oss-20b","profile":"p150x2","stop_serving":true}},"eta_s":300},
      {"key":"catalog:Qwen3-32B","group":"qwen3-32b","name":"Qwen3-32B","model":"Qwen/Qwen3-32B","source":"catalog","origin":"tt","profile":null,"profile_label":null,"profile_note":null,"chips":4,"context":null,"tools":{"verified":true,"how":"x"},"choice":{"action":"in_use"},"serve":null,"eta_s":600},
      {"key":"community:acme/gpt-oss-20b#p150x4","group":"gpt-oss-20b","name":"gpt-oss-20b","model":"openai/gpt-oss-20b","source":"community","origin":"acme","profile":"p150x4-throughput","profile_label":"Throughput","profile_note":"Up to 32 sequences.","chips":4,"context":null,"tools":null,"choice":{"action":"switch"},"serve":{"route":"/community/serve","body":{"id":"acme/gpt-oss-20b","profile":"p150x4-throughput","stop_serving":true}},"eta_s":null},
      {"key":"native:/opt/m","group":"llama","name":"Llama","model":"meta/Llama","source":"native","origin":"","profile":null,"profile_label":null,"profile_note":null,"chips":null,"context":null,"tools":null,"choice":{"action":"unavailable","why":"tt-model isn't installed on this box."},"serve":null,"eta_s":null}
     ]}
    """#

    func testEverythingTheBoxCanServeGroupsByModelInUseFirst() throws {
        let decoded = try JSONDecoder().decode(ServeOptions.self, from: Data(options.utf8))
        let groups = ServeGroups.make(decoded.models)
        XCTAssertEqual(groups.map(\.id), ["qwen3-32b", "gpt-oss-20b", "llama"])
        XCTAssertEqual(groups[1].options.map(\.profileLabel), ["Interactive", "Throughput"])
        XCTAssertTrue(groups[0].inUse)
        XCTAssertEqual(groups[2].options[0].blocked, "tt-model isn't installed on this box.")
        XCTAssertNil(groups[1].options[0].blocked)
        XCTAssertEqual(ServeGroups.detail(groups[1].options[0]), "Community · Interactive · 2 chips · 128K context")
        XCTAssertEqual(ServeGroups.make(decoded.models, query: "llama").map(\.id), ["llama"])

        // The serve call goes back exactly as the box gave it.
        let call = try XCTUnwrap(groups[1].options[1].serve)
        XCTAssertEqual(call.route, "/community/serve")
        let body = try JSONSerialization.jsonObject(with: JSONEncoder().encode(call.body)) as? [String: Any]
        XCTAssertEqual(body?["profile"] as? String, "p150x4-throughput")
        XCTAssertEqual(body?["stop_serving"] as? Bool, true)
    }

    func testPausingARoutineSendsTheRestBackUnchanged() throws {
        let json = #"{"id":"r1","name":"Morning brief","goal":"Summarize the news","schedule":{"kind":"weekdays","time":"07:30"},"when":"Weekdays at 07:30","allowed":["web_search"],"enabled":true,"created":1,"next_run":2,"mode":null,"last":null}"#
        let routine = try JSONDecoder().decode(Routine.self, from: Data(json.utf8))
        let update = try XCTUnwrap(RoutineUpdate(routine, enabled: false))
        let body = try JSONSerialization.jsonObject(with: JSONEncoder().encode(update)) as? [String: Any]
        XCTAssertEqual(body?["enabled"] as? Bool, false)
        XCTAssertEqual(body?["goal"] as? String, "Summarize the news")
        XCTAssertEqual(body?["allowed"] as? [String], ["web_search"])
        XCTAssertEqual((body?["schedule"] as? [String: Any])?["time"] as? String, "07:30")
        // An agent that doesn't say the schedule: no guessing.
        let bare = #"{"id":"r1","name":"x","goal":"y","when":"Manual","enabled":true,"next_run":null,"last":null}"#
        XCTAssertNil(RoutineUpdate(try JSONDecoder().decode(Routine.self, from: Data(bare.utf8)), enabled: false))
    }

    func testRecentFoldersAreNewestFirstAndUnique() {
        func s(_ id: String, _ folder: String, _ last: Double) -> CodeSession {
            CodeSession(id: id, folder: folder, title: id, last: last, sandbox: true, running: false, busy: false, waiting: 0, changes: 0)
        }
        let recent = RecentFolders.from([s("a", "/home/ttuser/code/app", 1), s("b", "/home/ttuser/code/site", 3),
                                         s("c", "/home/ttuser/code/app", 5)])
        XCTAssertEqual(recent, ["/home/ttuser/code/app", "/home/ttuser/code/site"])
        XCTAssertEqual(RecentFolders.short("/home/ttuser/code/app"), "~/code/app")
    }

    func testAResearchReportWithItsSources() throws {
        let json = ##"{"job":{"id":"j1","question":"How do Blackhole chips cool?","depth":"standard","created":1760500000,"state":"done","step":"Done","finished":1760500600,"error":null,"model":"Qwen/Qwen3-32B","search":"web","summary":"They use…","title":null,"round":1,"started_by_name":"Jane's iPhone","shared":false},"events":[],"report":"# Cooling\nFans [1].","sources":[{"n":1,"url":"https://example.com/a","title":"A","query":"q","status":"read","relevant":true}],"counts":{},"plan":null,"can_change":true}"##
        let report = try JSONDecoder().decode(ResearchReport.self, from: Data(json.utf8))
        XCTAssertEqual(report.sources.first?.n, 1)
        XCTAssertEqual(report.canChange, true)
        XCTAssertFalse(report.job.isActive)
        XCTAssertEqual(report.job.displayTitle, "How do Blackhole chips cool?")
    }

    func testARecordingInTheRoomSaysWhoStartedIt() throws {
        let json = #"{"microphone":"USB mic","speakers":"Speakers","recording":{"id":"r","title":"Standup","started":1760500000,"seconds":125,"mic":"USB mic","system":"Speakers","mic_db":-30.0,"system_db":null,"ended":false,"max_seconds":14400,"started_by":"Jane's iPhone"}}"#
        let status = try JSONDecoder().decode(RecordStatus.self, from: Data(json.utf8))
        XCTAssertEqual(status.recording?.startedBy, "Jane's iPhone")
        XCTAssertEqual(status.recording?.maxSeconds, 14400)
    }
}
