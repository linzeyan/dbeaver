import Foundation

/// One cell of a browse result, by its place in it.
///
/// Position and not identity, because that is what the grid has: a row is the
/// nth row of what was fetched. It survives editing only because a pending edit
/// is discarded whenever the result is re-read, which is also the moment the
/// database's own answer replaces what was typed.
struct GridCell: Hashable {
    let row: Int
    let column: Int
}

/// What a cell was changed to. `nil` is SQL's NULL, which is not an empty
/// string — a text column can hold both and a grid has to be able to say which.
struct PendingValue: Equatable {
    let text: String?
}

/// The changes a grid is holding, as the core's edit surface wants them.
///
/// Mirrors of `dbedit`'s types, encoding to the shape `db_edit_sql_json`
/// documents. Written out here rather than reused from the decode side because
/// nothing decodes them: this is the one direction, and a shared type would
/// invite a field to be added on the side that does not send it.
struct EditRequest: Encodable {
    let schema: String
    let relation: String
    var updates: [Update] = []
    var inserts: [Insert] = []
    var deletes: [Delete] = []

    struct Update: Encodable {
        let key: [Cell]
        let set: [Cell]
    }

    struct Insert: Encodable {
        let set: [Cell]
    }

    struct Delete: Encodable {
        let key: [Cell]
    }

    struct Cell: Encodable {
        let column: String
        let value: String?
    }

    var json: String {
        let data = (try? JSONEncoder().encode(self)) ?? Data()
        return String(data: data, encoding: .utf8) ?? "{}"
    }
}
