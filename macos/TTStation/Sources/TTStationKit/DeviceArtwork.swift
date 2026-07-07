import Foundation

/// Maps a box's detected device-mesh label to a product-artwork asset name
/// (an image set in the app bundle's asset catalog), or `nil` when there's no
/// artwork for that mesh.
///
/// Today only the **QuietBox 2** has a product image: it's the `p300x2` mesh
/// (4× `p300c` Blackhole cards — see the agent's `detect_device_mesh`), which
/// maps to the `QuietBox2` image set. Matching is case-insensitive and
/// tolerates a bare `p300` reported without the card-count suffix.
///
/// NOTE: a Loudbox also presents as `p300x2` to the mesh detector; this app is
/// QuietBox-2-focused, so `p300x2 → QuietBox 2` is the intended reading. If a
/// Loudbox ever needs to be told apart, that requires a signal beyond the mesh
/// (the agent doesn't report a chassis model today).
public enum DeviceArtwork {
    /// The asset-catalog image name for `mesh`, or `nil` if none applies.
    public static func assetName(forMesh mesh: String?) -> String? {
        guard let mesh = mesh?.lowercased(), !mesh.isEmpty else { return nil }
        switch mesh {
        case "p300x2", "p300": return "QuietBox2"
        default: return nil
        }
    }
}
