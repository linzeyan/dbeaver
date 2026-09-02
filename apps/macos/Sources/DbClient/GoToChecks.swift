import Foundation

/// Executable checks for the go-to palette's matching and for what a window puts
/// in it, run by `--verify-goto`.
///
/// The rule is `catalog::rank`'s and lives in two places, which is the whole
/// reason these exist: the Rust side is tested where it is written, and this
/// side has to be pinned by behaviour so that the two drifting apart shows up
/// here rather than as a palette that ranks a name differently from the
/// completion offering the same name.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum GoToChecks {
    private static var failures = 0

    static func run() -> Bool {
        // The cases that build a real `AppModel` read the saved connections, and
        // ask the Keychain about the first — which blocks for ever in a process
        // with no GUI session. `BrowseRestoreChecks` says the same thing at more
        // length.
        guard let scratch = scratchDirectory() else { return false }
        defer { try? FileManager.default.removeItem(at: scratch) }
        setenv("XDG_CONFIG_HOME", scratch.path, 1)
        defer { ScratchDefaults.release() }

        failures = 0
        checkAnEmptyNeedleListsEverythingInOrder()
        checkNamesThatBeginWithItComeFirst()
        checkADotSeparatesSchemaFromName()
        checkMatchingIgnoresCase()
        checkASavedQueryIsFoundByItsName()
        checkATableOutranksASavedQueryOfTheSameName()
        checkTheConnectionInFrontComesBeforeTheOnesBehindIt()
        checkATableOutranksAConnectionOfTheSameName()
        checkEveryTabsTablesAreInTheList()
        checkTheOtherConnectionsAreOfferedAndThisOneIsNot()
        checkOneConnectionIsOfferedNoConnections()
        checkGoingToAConnectionPutsItInFront()
        checkGoingToATableBehindPutsItsTabInFrontFirst()
        checkThereIsSomewhereToGoWhenAnyTabHasSomethingInIt()
        checkARowNamingATabTheWindowNoLongerHasOpensNothing()
        if failures == 0 {
            fputs("goto: all checks passed\n", stderr)
        } else {
            fputs("goto: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The palette opens with everything in it, in one predictable order.
    ///
    /// Ordered by the qualified name and not by the bare one: two schemas can
    /// hold a table of the same name, and a list whose order depends on which
    /// arrived first would put them in a different place each session.
    private static func checkAnEmptyNeedleListsEverythingInOrder() {
        expect(
            found(""),
            [
                "public.Customers", "public.customer_orders", "public.orders", "sales.orders",
                "sales.regions"
            ],
            "an empty needle is every table, by qualified name")
    }

    /// A name that begins with the text is offered before one that merely holds
    /// it, which is the half of the rule that makes typing three letters useful.
    private static func checkNamesThatBeginWithItComeFirst() {
        expect(
            found("orders"), ["public.orders", "sales.orders", "public.customer_orders"],
            "the two called orders come before the one containing it")
    }

    /// A dot is read as schema-then-name, the way the SQL completion reads it.
    ///
    /// The trailing-dot case is the one somebody is actually in while typing:
    /// `sales.` has named a schema and nothing in it, and listing that schema is
    /// the answer — an empty list would read as "this schema is empty".
    private static func checkADotSeparatesSchemaFromName() {
        expect(found("sales.ord"), ["sales.orders"], "both halves narrow the list")
        expect(
            found("sales."), ["sales.orders", "sales.regions"],
            "a trailing dot lists the schema")
    }

    /// Case is ignored on both sides. A database whose tables are named in mixed
    /// case is not a database somebody wants to hold Shift for.
    private static func checkMatchingIgnoresCase() {
        expect(
            found("CUST"), ["public.Customers", "public.customer_orders"],
            "an upper-case needle finds lower-case names and the other way round")
    }

    /// A saved query is reached by a substring of its name, exactly as a table
    /// is. The palette is one list, and a second way of finding things in it
    /// would be a second thing to remember.
    private static func checkASavedQueryIsFoundByItsName() {
        expect(
            found("rollup", in: mixed), ["nightly rollup"],
            "the saved query is in the same list as the tables")
    }

    /// A table beats a saved query of the same name at the same match strength,
    /// and a merely containing match still comes after both.
    private static func checkATableOutranksASavedQueryOfTheSameName() {
        expect(
            found("orders", in: mixed),
            ["public.orders", "sales.orders", "orders", "public.customer_orders"],
            "the two tables called orders come before the saved query of that name")
    }

    /// The table in the tab somebody is looking at is offered before the table
    /// of that name in the tab behind it.
    ///
    /// A window holding prod and staging has the same table in both, and the
    /// question the palette answers is which one is meant. The one on screen
    /// is; the row for the other says so, which is the only reason a palette
    /// over several databases is usable at all.
    private static func checkTheConnectionInFrontComesBeforeTheOnesBehindIt() {
        let behind = GoToTarget(schema: "public", name: "orders", tab: 1, connection: "staging")
        let front = GoToTarget(schema: "public", name: "orders")
        expect(
            GoTo.ranked([behind, front], matching: "orders").map(\.connection), ["", "staging"],
            "the table in the tab in front comes first")
        // And the ones behind are grouped by the connection they are in rather
        // than interleaved by name: a list that alternated between two servers
        // would make somebody read the label on every row.
        let other = GoToTarget(schema: "public", name: "orders", tab: 2, connection: "prod")
        expect(
            GoTo.ranked([behind, other, front], matching: "").map(\.connection),
            ["", "prod", "staging"], "and the rest are grouped by connection")
    }

    /// A table beats a connection of the same name, and both beat the saved
    /// query. The palette is opened over a database, and a table is what
    /// somebody typing into it means.
    private static func checkATableOutranksAConnectionOfTheSameName() {
        let list = [
            GoToTarget(schema: "", name: "orders", kind: .favorite, sql: "select 1"),
            GoToTarget(schema: "", name: "orders", kind: .connection, tab: 1),
            GoToTarget(schema: "public", name: "orders")
        ]
        expect(
            GoTo.ranked(list, matching: "orders").map(\.kind),
            [.relation, .connection, .favorite],
            "the table, then the connection called that, then the saved query")
    }

    // MARK: - Cases over a window

    /// Every tab's tables are in the list, and the ones behind say where they
    /// are.
    private static func checkEveryTabsTablesAreInTheList() {
        let model = twoTabbedModel()
        let relations = model.goToTargets.filter { $0.kind == .relation }
        expect(
            relations.map(\.qualified),
            ["public.orders", "public.orders", "public.regions"],
            "the tables read in both tabs are offered")
        expect(
            relations.map(\.connection), ["", "staging", "staging"],
            "and the two behind are named for the connection they are in")
        expect(
            relations.map(\.scheme), ["postgres", "mysql", "mysql"],
            "each carrying its own driver, for the mark at the end of the row")
    }

    /// The other connections are rows and the one in front is not — a row that
    /// took you where you already are is a row that does nothing.
    private static func checkTheOtherConnectionsAreOfferedAndThisOneIsNot() {
        let model = twoTabbedModel()
        expect(
            model.goToTargets.filter { $0.kind == .connection }.map(\.name), ["staging"],
            "the tab behind is offered and the tab in front is not")
    }

    /// A window with one connection offers none. Which is the same rule as the
    /// case above and is worth its own check, because it is the shape almost
    /// every window is in and the one a regression would show up in first.
    private static func checkOneConnectionIsOfferedNoConnections() {
        let model = emptyWindow(tabs: 1)
        fill(model.sessions[0], "local", "postgres://localhost/app", ["orders"])
        expect(
            model.goToTargets.map(\.kind), [.relation],
            "the only connection open is not somewhere to go")
    }

    /// Choosing a connection puts that tab in front, and does nothing else.
    private static func checkGoingToAConnectionPutsItInFront() {
        let model = twoTabbedModel()
        expect(model.activeSession, 0, "the window starts on its first tab")
        guard let row = model.goToTargets.first(where: { $0.kind == .connection }) else {
            failures += 1
            fputs("goto FAIL: the fixture offered no connection to go to\n", stderr)
            return
        }
        model.isGoToOpen = true
        model.goTo(row)
        expect(model.activeSession, 1, "the connection row moved the window to that tab")
        expect(model.isGoToOpen, false, "and closed the palette behind it")
        expect(model.selected == nil, true, "without opening anything in it")
    }

    /// A table in the tab behind brings its tab forward first.
    ///
    /// The order matters and is the reason `goTo` is written the way it is:
    /// everything below that line reads whichever tab is in front, so a
    /// selection made before the switch would land on the wrong connection —
    /// and land silently, because both tabs have a `public`.
    private static func checkGoingToATableBehindPutsItsTabInFrontFirst() {
        let model = twoTabbedModel()
        guard
            let row = model.goToTargets.first(where: {
                $0.kind == .relation && $0.name == "regions"
            })
        else {
            failures += 1
            fputs("goto FAIL: the fixture offered no table in the tab behind\n", stderr)
            return
        }
        model.goTo(row)
        expect(model.activeSession, 1, "the tab holding it came forward")
        expect(model.selected?.name, "regions", "with the table selected")
        expect(model.activeTab, .content, "on the pane that shows its rows")
        expect(
            model.sessions[0].selected == nil, true,
            "and nothing selected in the tab that was in front")
    }

    /// A row naming a tab that is not there opens nothing.
    ///
    /// The failure this rules out is the quiet one. Every connection has a
    /// `public`, so a row looked up in whichever tab happened to be in front
    /// would find a table of that name on the wrong server and show its rows
    /// with nothing on screen saying so.
    private static func checkARowNamingATabTheWindowNoLongerHasOpensNothing() {
        let model = twoTabbedModel()
        model.goTo(GoToTarget(schema: "public", name: "orders", tab: 5))
        expect(model.activeSession, 0, "the window stayed where it was")
        expect(model.selected == nil, true, "and opened nothing on the tab it is on")
    }

    /// Whether the menu offers the palette at all: yes as soon as any tab has
    /// read a tree, and yes for a second connection whether or not it has.
    ///
    /// The second half is the one worth writing down. A window with two tabs
    /// and nothing read in either still has somewhere to go — the other tab —
    /// and a command greyed out there would be greyed out for a reason nobody
    /// could see, with a working destination on screen beside it.
    private static func checkThereIsSomewhereToGoWhenAnyTabHasSomethingInIt() {
        expect(
            emptyWindow(tabs: 1).canGoTo, false,
            "a window that has read nothing has nowhere to go")
        expect(
            emptyWindow(tabs: 2).canGoTo, true,
            "a second connection is somewhere to go before anything is read")
        let read = emptyWindow(tabs: 1)
        fill(read.sessions[0], "local", "postgres://localhost/app", ["orders"])
        expect(read.canGoTo, true, "and so are the tables one tab has read")
    }

    // MARK: - Fixture

    /// Two schemas, a name held by both, a name containing another, and one
    /// name that is not lower case — the four shapes the ordering has to
    /// separate.
    private static let targets = [
        GoToTarget(schema: "public", name: "orders"),
        GoToTarget(schema: "public", name: "customer_orders"),
        GoToTarget(schema: "public", name: "Customers"),
        GoToTarget(schema: "sales", name: "orders"),
        GoToTarget(schema: "sales", name: "regions")
    ]

    /// The same five tables with two saved queries among them: one named for a
    /// table that exists, which is what the ordering has to separate, and one
    /// named for nothing in the database at all.
    private static let mixed =
        targets + [
            GoToTarget(schema: "", name: "orders", kind: .favorite, sql: "select * from orders"),
            GoToTarget(
                schema: "", name: "nightly rollup", kind: .favorite,
                sql: "select date, sum(total) from orders group by 1")
        ]

    private static func found(_ needle: String, in list: [GoToTarget] = targets) -> [String] {
        GoTo.ranked(list, matching: needle).map(\.qualified)
    }

    /// A window with two connections open: `local` in front holding one table,
    /// and `staging` behind it holding two — one of them named the same, which
    /// is the case the connection label on a row exists for.
    private static func twoTabbedModel() -> AppModel {
        let model = emptyWindow(tabs: 2)
        fill(model.sessions[0], "local", "postgres://localhost/app", ["orders"])
        fill(model.sessions[1], "staging", "mysql://staging/app", ["orders", "regions"])
        return model
    }

    /// A window of that many tabs, none of them connected. Built through
    /// restore, which is the one way to make a model with more than one tab
    /// without a server to open them against.
    private static func emptyWindow(tabs: Int) -> AppModel {
        let tab = RestoredTab(
            connection: nil, settings: nil, label: "", buffers: [], activeBuffer: 0)
        return AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-goto")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-goto")),
            preferences: Preferences(store: ScratchDefaults.store("verify-goto")),
            restoring: RestoredWindow(
                tabs: Array(repeating: tab, count: tabs), activeTab: 0))
    }

    /// Puts a name, a driver and a read tree on a tab — the state a palette is
    /// opened over. No connection: the palette lists what is already in the
    /// window's inventory, which is exactly why it can be checked without one.
    private static func fill(
        _ session: Session, _ label: String, _ url: String, _ tables: [String]
    ) {
        session.connectionLabel = label
        session.connString = url
        session.schemas = [SchemaInfo(name: "public", isSystem: false)]
        session.relations = [
            "public": tables.map {
                RelationInfo(schema: "public", name: $0, kind: .table, estimatedRows: nil)
            }
        ]
    }

    /// A directory of its own for the config these checks must not read.
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-goto-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs("goto FAIL: a scratch directory could not be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("goto FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
