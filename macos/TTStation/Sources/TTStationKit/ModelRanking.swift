import Foundation

/// Pure hardware-aware ranking of a box's servable models.
///
/// Splits `[ModelInfo]` into a **compatible** tier (models whose declared
/// `devices` include this box's detected mesh) and an **incompatible** tier
/// (everything else, annotated with the hardware it needs). The compatible tier
/// is family-grouped for display and its families/models keep `ModelDefaults`'
/// existing ordering. When `boxMesh` is `nil` (mesh unknown) there is no basis
/// to split, so every model is treated as compatible.
public enum ModelRanking {
    public struct RankedModels: Equatable {
        public let compatible: [(family: String, models: [ModelInfo])]
        public let incompatible: [ModelInfo]

        public static func == (l: RankedModels, r: RankedModels) -> Bool {
            l.incompatible == r.incompatible
                && l.compatible.map(\.family) == r.compatible.map(\.family)
                && l.compatible.map(\.models) == r.compatible.map(\.models)
        }
    }

    /// Case-insensitive membership of `boxMesh` in a model's device meshes.
    /// `nil` boxMesh matches nothing (caller decides how to treat unknown).
    public static func meshMatches(_ devices: [String], boxMesh: String?) -> Bool {
        guard let boxMesh else { return false }
        return devices.contains { $0.caseInsensitiveCompare(boxMesh) == .orderedSame }
    }

    public static func rankForHardware(_ models: [ModelInfo], boxMesh: String?) -> RankedModels {
        guard let boxMesh else {
            return RankedModels(
                compatible: ModelDefaults.groupModelsByFamily(models),
                incompatible: [])
        }
        let compatible = models.filter { meshMatches($0.devices, boxMesh: boxMesh) }
        let incompatible = models
            .filter { !meshMatches($0.devices, boxMesh: boxMesh) }
            .sorted { $0.name < $1.name }
        return RankedModels(
            compatible: ModelDefaults.groupModelsByFamily(compatible),
            incompatible: incompatible)
    }

    /// A short human label: `"Runs on P300X2"` for a compatible model,
    /// `"Needs <mesh, mesh>"` for an incompatible one, `""` when mesh unknown.
    public static func compatibilityLabel(for model: ModelInfo, boxMesh: String?) -> String {
        guard let boxMesh else { return "" }
        if meshMatches(model.devices, boxMesh: boxMesh) {
            return "Runs on \(boxMesh.uppercased())"
        }
        return "Needs \(model.devices.joined(separator: ", "))"
    }
}
