import XCTest
@testable import TTStationKit

final class ModelsTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let url = Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures")
        return try Data(contentsOf: try XCTUnwrap(url))
    }

    func testDecodeDiscover() throws {
        let boxes = try JSONDecoder().decode([BoxRecord].self, from: fixture("discover"))
        XCTAssertEqual(boxes.count, 1)
        XCTAssertEqual(boxes[0].name, "quietbox2")
        XCTAssertEqual(boxes[0].ctrlPort, 8080)
        XCTAssertEqual(boxes[0].status, .serving(model: "Qwen3-8B"))
    }

    func testDecodeModelsWithReleaseVersion() throws {
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: fixture("models"))
        XCTAssertEqual(resp.releaseVersion, "0.14.0")
        XCTAssertEqual(resp.models.map(\.name), ["Qwen3-8B", "Llama-3.1-8B-Instruct"])
        XCTAssertEqual(resp.models[1].devices, ["P300X2", "T3K"])
    }

    func testDecodeModelsNullReleaseVersion() throws {
        let data = Data(#"{"release_version":null,"models":[]}"#.utf8)
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: data)
        XCTAssertNil(resp.releaseVersion)
        XCTAssertTrue(resp.models.isEmpty)
    }

    func testBoxRecordDecodesDeviceMesh() throws {
        let json = #"{"name":"qb2","host":"qb2-lab.local","ctrl_port":8765,"chips":"4xBH","status":"idle","apiver":1,"device_mesh":"p300x2"}"#
        let rec = try JSONDecoder().decode(BoxRecord.self, from: Data(json.utf8))
        XCTAssertEqual(rec.deviceMesh, "p300x2")
    }

    func testBoxRecordDeviceMeshDefaultsNilWhenAbsent() throws {
        let json = #"{"name":"qb2","host":"qb2-lab.local","ctrl_port":8765,"chips":"4xBH","status":"idle","apiver":1}"#
        let rec = try JSONDecoder().decode(BoxRecord.self, from: Data(json.utf8))
        XCTAssertNil(rec.deviceMesh)
    }

    func testHostPortStripsTrailingDot() {
        let dotted = BoxRecord(name: "b", host: "qb2-lab.local.", ctrlPort: 8765, chips: "x", statusRaw: "idle", apiver: 1)
        XCTAssertEqual(dotted.hostPort, "qb2-lab.local:8765")

        let plain = BoxRecord(name: "b", host: "qb2-lab.local", ctrlPort: 8765, chips: "x", statusRaw: "idle", apiver: 1)
        XCTAssertEqual(plain.hostPort, "qb2-lab.local:8765")
    }

    func testDecodeEndpoint() throws {
        let ep = try JSONDecoder().decode(Endpoint.self, from: fixture("endpoint"))
        XCTAssertEqual(ep.baseURL, "http://192.168.5.119:8000/v1")
        XCTAssertEqual(ep.model, "Qwen3-8B")
        XCTAssertFalse(ep.requiresKey)
    }

    func testDecodeServing() throws {
        let list = try JSONDecoder().decode(ServingList.self, from: fixture("serving"))
        XCTAssertEqual(list.serving.count, 2)

        let agent = list.serving[0]
        XCTAssertEqual(agent.model, "Qwen3-8B")
        XCTAssertEqual(agent.baseURL, "http://192.168.5.119:8000/v1")
        XCTAssertEqual(agent.hostPort, 8000)
        XCTAssertEqual(agent.container, "tt-inference-qwen3-8b")
        XCTAssertEqual(agent.source, "agent")

        let external = list.serving[1]
        XCTAssertEqual(external.model, "Llama-3.1-70B-Instruct")
        XCTAssertEqual(external.hostPort, 8001)
        XCTAssertEqual(external.source, "external")
    }

    func testDecodeServingEmpty() throws {
        let list = try JSONDecoder().decode(ServingList.self, from: Data(#"{"serving":[]}"#.utf8))
        XCTAssertTrue(list.serving.isEmpty)
    }

    func testDecodeBoxConfig() throws {
        let json = #"""
        {
            "active_profile": "stable",
            "available_profiles": ["stable", "bleeding"],
            "backend": "runpy",
            "serving_host": "qb2-lab.local",
            "serving_port": 8003,
            "serving_image": "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0",
            "tt_inference_repo": "/home/ttuser/code/tt-inference-server",
            "tt_device": "p300x2"
        }
        """#
        let cfg = try JSONDecoder().decode(BoxConfig.self, from: Data(json.utf8))
        XCTAssertEqual(cfg.activeProfile, "stable")
        XCTAssertEqual(cfg.availableProfiles, ["stable", "bleeding"])
        XCTAssertEqual(cfg.backend, "runpy")
        XCTAssertEqual(cfg.servingHost, "qb2-lab.local")
        XCTAssertEqual(cfg.servingPort, 8003)
        XCTAssertEqual(cfg.servingImage, "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0")
        XCTAssertEqual(cfg.ttInferenceRepo, "/home/ttuser/code/tt-inference-server")
        XCTAssertEqual(cfg.ttDevice, "p300x2")
    }

    func testDecodeBoxConfigNullsBecomeNilOrEmpty() throws {
        let json = #"""
        {
            "active_profile": null,
            "available_profiles": [],
            "backend": "runpy",
            "serving_host": "127.0.0.1",
            "serving_port": 8000,
            "serving_image": null,
            "tt_inference_repo": null,
            "tt_device": null
        }
        """#
        let cfg = try JSONDecoder().decode(BoxConfig.self, from: Data(json.utf8))
        XCTAssertNil(cfg.activeProfile)
        XCTAssertTrue(cfg.availableProfiles.isEmpty)
        XCTAssertEqual(cfg.backend, "runpy")
        XCTAssertEqual(cfg.servingHost, "127.0.0.1")
        XCTAssertEqual(cfg.servingPort, 8000)
        XCTAssertNil(cfg.servingImage)
        XCTAssertNil(cfg.ttInferenceRepo)
        XCTAssertNil(cfg.ttDevice)
    }

    func testDecodeBoxCatalog() throws {
        let json = #"""
        {
            "box_mesh": "p300x2",
            "catalog_available": true,
            "catalog_stale": false,
            "runs_here": [
                {
                    "id": "qwen3-8b",
                    "display_name": "Qwen3-8B",
                    "family": "Qwen3",
                    "size": "8B",
                    "software": ["vllm"],
                    "meshes": ["p300x2"],
                    "needed_hardware": [],
                    "available_now": true,
                    "status_here": "supported"
                }
            ],
            "experimental": [
                {
                    "id": "llama-3.1-70b",
                    "display_name": "Llama-3.1-70B",
                    "family": "Llama",
                    "size": "70B",
                    "software": ["vllm"],
                    "meshes": ["t3k"],
                    "needed_hardware": [],
                    "available_now": false,
                    "status_here": "experimental"
                }
            ],
            "other_hardware": [
                {
                    "id": "llama-3.1-405b",
                    "display_name": "Llama-3.1-405B",
                    "family": "Llama",
                    "size": "405B",
                    "software": ["vllm"],
                    "meshes": ["t3k"],
                    "needed_hardware": ["T3K"],
                    "available_now": false,
                    "status_here": "needs_other_hardware"
                }
            ]
        }
        """#
        let catalog = try JSONDecoder().decode(BoxCatalog.self, from: Data(json.utf8))
        XCTAssertEqual(catalog.boxMesh, "p300x2")
        XCTAssertTrue(catalog.catalogAvailable)
        XCTAssertFalse(catalog.catalogStale)

        XCTAssertEqual(catalog.runsHere.count, 1)
        let runsHere = catalog.runsHere[0]
        XCTAssertEqual(runsHere.id, "qwen3-8b")
        XCTAssertEqual(runsHere.displayName, "Qwen3-8B")
        XCTAssertEqual(runsHere.family, "Qwen3")
        XCTAssertEqual(runsHere.size, "8B")
        XCTAssertEqual(runsHere.software, ["vllm"])
        XCTAssertEqual(runsHere.meshes, ["p300x2"])
        XCTAssertEqual(runsHere.neededHardware, [])
        XCTAssertTrue(runsHere.availableNow)
        XCTAssertEqual(runsHere.statusHere, "supported")

        XCTAssertEqual(catalog.experimental.count, 1)
        XCTAssertEqual(catalog.experimental[0].id, "llama-3.1-70b")
        XCTAssertFalse(catalog.experimental[0].availableNow)

        XCTAssertEqual(catalog.otherHardware.count, 1)
        XCTAssertEqual(catalog.otherHardware[0].neededHardware, ["T3K"])
    }

    func testDecodeBoxCatalogNullBoxMesh() throws {
        let json = #"""
        {
            "box_mesh": null,
            "catalog_available": false,
            "catalog_stale": true,
            "runs_here": [],
            "experimental": [],
            "other_hardware": []
        }
        """#
        let catalog = try JSONDecoder().decode(BoxCatalog.self, from: Data(json.utf8))
        XCTAssertNil(catalog.boxMesh)
        XCTAssertFalse(catalog.catalogAvailable)
        XCTAssertTrue(catalog.catalogStale)
        XCTAssertTrue(catalog.runsHere.isEmpty)
        XCTAssertTrue(catalog.experimental.isEmpty)
        XCTAssertTrue(catalog.otherHardware.isEmpty)
    }
}
