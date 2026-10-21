import Foundation

/// Serializes a v2 control-socket request with its `method` key first.
///
/// The receiver now waits for the complete newline-framed request before
/// routing it. Keeping `method` first still gives these writes a stable key
/// order, but clients no longer depend on it to cross the route-inspection
/// window. `JSONSerialization` writes dictionary keys in hash order, which
/// differs from one process to the next (#21222).
/// lint:allow namespace-type: stateless wire encoding helper with no instance state.
public struct ControlRequestLine {
    private init() {}

    /// The request could not be written as JSON.
    public struct EncodingError: Error, Equatable {
        public init() {}
    }

    /// One JSON object for `request`, without a trailing newline: the
    /// `method` key first, then the other keys in sorted order.
    ///
    /// - Throws: ``EncodingError`` when `request` is not a valid JSON object.
    public static func encode(_ request: [String: Any]) throws -> String {
        guard JSONSerialization.isValidJSONObject(request) else { throw EncodingError() }
        guard let method = request["method"] else {
            return try string(JSONSerialization.data(withJSONObject: request, options: [.sortedKeys]))
        }
        var remainder = request
        remainder.removeValue(forKey: "method")
        let head = try JSONSerialization.data(withJSONObject: ["method": method], options: [])
        guard !remainder.isEmpty else { return try string(head) }
        let tail = try JSONSerialization.data(withJSONObject: remainder, options: [.sortedKeys])
        // `{"method":…}` and `{…}` joined as `{"method":…,…}`.
        var line = Data(head.dropLast())
        line.append(UInt8(ascii: ","))
        line.append(contentsOf: tail.dropFirst())
        return try string(line)
    }

    private static func string(_ data: Data) throws -> String {
        guard let line = String(data: data, encoding: .utf8) else { throw EncodingError() }
        return line
    }
}
