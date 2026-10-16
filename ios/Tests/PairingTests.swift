import XCTest
@testable import TTStationMobile

/// The scanned `ttstation://pair` link (`PairLink.parse`, used by `AppModel.handle(url:)`).
final class PairLinkTests: XCTestCase {
    private let fp = String(repeating: "ab", count: 32)

    private func parse(_ link: String) -> Result<PairLink, PairLink.Problem> {
        guard let url = URL(string: link) else {
            XCTFail("not a URL: \(link)")
            return .failure(.notPairing)
        }
        return PairLink.parse(url)
    }

    func testAFullLinkCarriesEverythingToPairWith() throws {
        let link = try parse(
            "ttstation://pair?host=10.0.0.122&pair=p-123&code=482913&tls=8443&fp=\(fp)&port=8766&name=qb2-lab&ssh=ttuser"
        ).get()
        XCTAssertEqual(link, PairLink(host: "10.0.0.122", pairID: "p-123", code: "482913",
                                      pin: TLSPin(port: 8443, sha256: fp), port: 8766,
                                      name: "qb2-lab", sshUser: "ttuser"))
    }

    func testTheControlPortNameAndSSHUserAreOptional() throws {
        let link = try parse("ttstation://pair?host=qb2.local&pair=p&code=1&tls=8443&fp=\(fp)").get()
        XCTAssertEqual(link.port, 8765)
        XCTAssertEqual(link.name, "qb2.local")
        XCTAssertNil(link.sshUser)
        // Present but empty counts as absent.
        let empty = try parse("ttstation://pair?host=h&pair=p&code=1&tls=8443&fp=\(fp)&name=&ssh=").get()
        XCTAssertEqual(empty.name, "h")
        XCTAssertNil(empty.sshUser)
    }

    func testAnUppercaseFingerprintIsPinnedInLowercase() throws {
        let link = try parse("ttstation://pair?host=h&pair=p&code=1&tls=8443&fp=\(fp.uppercased())").get()
        XCTAssertEqual(link.pin.sha256, fp)
    }

    func testAnIPv6HostKeepsItsZone() throws {
        let link = try parse("ttstation://pair?host=fe80::1%25en0&pair=p&code=1&tls=8443&fp=\(fp)").get()
        XCTAssertEqual(link.host, "fe80::1%en0")
    }

    func testALinkWithoutTheHostPairOrCodeIsIncomplete() {
        for link in [
            "ttstation://pair?pair=p&code=1&tls=8443&fp=\(fp)",
            "ttstation://pair?host=h&code=1&tls=8443&fp=\(fp)",
            "ttstation://pair?host=h&pair=p&tls=8443&fp=\(fp)",
            "ttstation://pair?host=h&pair=p&code=&tls=8443&fp=\(fp)",
        ] {
            XCTAssertEqual(parse(link), .failure(.incomplete), link)
        }
    }

    func testALinkWithoutAUsableCertificateIsRefused() {
        let base = "ttstation://pair?host=h&pair=p&code=1"
        for rest in [
            "&fp=\(fp)",
            "&tls=8443",
            "&tls=0&fp=\(fp)",
            "&tls=65536&fp=\(fp)",
            "&tls=https&fp=\(fp)",
            "&tls=8443&fp=\(fp.dropLast())",
            "&tls=8443&fp=\(fp)a",
            "&tls=8443&fp=\(String(repeating: "zz", count: 32))",
        ] {
            XCTAssertEqual(parse(base + rest), .failure(.noCertificate), rest)
        }
    }

    func testOtherLinksAreNotPairingLinks() {
        for link in [
            "https://qb2.local/pair?host=h&pair=p&code=1&tls=8443&fp=\(fp)",
            "ttstation://open?host=h&pair=p&code=1&tls=8443&fp=\(fp)",
            "ttstation://pair",
        ] {
            XCTAssertEqual(parse(link), .failure(.notPairing), link)
        }
    }

    func testFingerprintsAre64LowercaseHexDigits() {
        XCTAssertTrue(PairLink.isSHA256Hex(fp))
        XCTAssertTrue(PairLink.isSHA256Hex(String(repeating: "0123456789abcdef", count: 4)))
        XCTAssertFalse(PairLink.isSHA256Hex(fp.uppercased()))
        XCTAssertFalse(PairLink.isSHA256Hex(String(fp.dropLast())))
        XCTAssertFalse(PairLink.isSHA256Hex(""))
    }
}

/// The box's certificate, pinned at pairing (`PinnedSession`).
final class CertificatePinTests: XCTestCase {
    // SHA-256("abc").
    private let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

    func testOnlyTheCertificateWithThePinnedHashMatches() {
        XCTAssertTrue(PinnedSession.matches(certificateDER: Data("abc".utf8), sha256: abc))
        XCTAssertTrue(PinnedSession.matches(certificateDER: Data("abc".utf8), sha256: abc.uppercased()))
        XCTAssertFalse(PinnedSession.matches(certificateDER: Data("abd".utf8), sha256: abc))
        XCTAssertFalse(PinnedSession.matches(certificateDER: Data(), sha256: abc))
        XCTAssertFalse(PinnedSession.matches(certificateDER: Data("abc".utf8), sha256: ""))
    }

    func testEachPinHasOneSession() {
        let other = String(repeating: "cd", count: 32)
        XCTAssertTrue(PinnedSession.session(sha256: abc) === PinnedSession.session(sha256: abc))
        XCTAssertFalse(PinnedSession.session(sha256: abc) === PinnedSession.session(sha256: other))
    }
}

/// Where the app sends requests (`AgentClient`, `SavedBox`).
final class AddressTests: XCTestCase {
    private let pin = TLSPin(port: 8443, sha256: String(repeating: "ab", count: 32))

    func testHostsAreWrittenAsAURLNeedsThem() {
        XCTAssertEqual(AgentClient.authority("10.0.0.5"), "10.0.0.5")
        XCTAssertEqual(AgentClient.authority("qb2-lab.local"), "qb2-lab.local")
        XCTAssertEqual(AgentClient.authority("::1"), "[::1]")
        XCTAssertEqual(AgentClient.authority("fe80::1%en0"), "[fe80::1%25en0]")
        XCTAssertEqual(AgentClient.authority("[::1]"), "[::1]")
    }

    func testRequestsGoOverHTTPSToTheBoxsTLSPort() throws {
        let client = try AgentClient(host: "10.0.0.5", pin: pin, token: "t")
        XCTAssertEqual(client.baseURL.absoluteString, "https://10.0.0.5:8443")
        XCTAssertEqual(client.address, "10.0.0.5:8443")
        XCTAssertEqual(client.token, "t")
        let v6 = try AgentClient(host: "::1", pin: pin)
        XCTAssertEqual(v6.baseURL.absoluteString, "https://[::1]:8443")
    }

    func testABoxPairedBeforeHTTPSMustPairAgain() {
        var box = SavedBox(name: "old", host: "10.0.0.5", port: 8765)
        XCTAssertNil(box.pin)
        XCTAssertThrowsError(try AgentClient(box: box, token: "t")) { error in
            guard case AgentError.insecure = error else {
                return XCTFail("expected .insecure, got \(error)")
            }
        }
        box.tlsPort = 8443
        XCTAssertNil(box.pin, "a port without a fingerprint isn't a pin")
        box.certSHA256 = pin.sha256
        XCTAssertEqual(box.pin, pin)
        XCTAssertEqual(box.id, "10.0.0.5:8765")
        XCTAssertNoThrow(try AgentClient(box: box, token: "t"))
    }

    func testSavedBoxesFromOlderVersionsStillDecode() throws {
        let json = #"{"name":"lab","host":"10.0.0.5","port":8765,"remoteHost":"lab.example.com"}"#
        let box = try JSONDecoder().decode(SavedBox.self, from: Data(json.utf8))
        XCTAssertEqual(box.host, "10.0.0.5")
        XCTAssertNil(box.pin)
        XCTAssertNil(box.sshUser)
    }

    func testErrorsSayWhatToDo() {
        XCTAssertEqual(AgentError.http(401, "unauthorized").errorDescription,
                       "This phone isn't paired with the box any more. Pair again.")
        XCTAssertEqual(AgentError.http(500, "boom").errorDescription, "boom")
        XCTAssertTrue(AgentError.badHost("a b").errorDescription?.contains("\"a b\"") ?? false)
        XCTAssertTrue(AgentError.insecure.errorDescription?.hasPrefix("Pair again") ?? false)
    }
}

final class DiscoveryHostTests: XCTestCase {
    func testScopedAndLinkLocalAddressesBecomeTheLocalName() {
        XCTAssertEqual(Discovery.usableHost("fe80::1c2b:3aff:fe4d:5e6f%en0", boxName: "tt-quietbox"), "tt-quietbox.local")
        XCTAssertEqual(Discovery.usableHost("FE80::1", boxName: "tt-quietbox"), "tt-quietbox.local")
        XCTAssertEqual(Discovery.usableHost("[fe80::1]", boxName: "tt-quietbox"), "tt-quietbox.local")
    }

    func testRoutableAddressesAndNamesStay() {
        XCTAssertEqual(Discovery.usableHost("10.0.0.122", boxName: "tt-quietbox"), "10.0.0.122")
        XCTAssertEqual(Discovery.usableHost("tt-quietbox.local", boxName: "tt-quietbox"), "tt-quietbox.local")
        XCTAssertEqual(Discovery.usableHost("2001:db8::2", boxName: "tt-quietbox"), "2001:db8::2")
    }
}
