import Foundation
import Observation
import SwiftUI

/// One open connection, and the state that belongs to it rather than to the
/// window around it.
///
/// The split is by lifetime. A property lives here if disconnecting would make
/// it wrong, and stays on `AppModel` if it describes the window instead: the
/// connection form's draft is the window's, because it is what somebody is
/// typing whichever database is open; the label in the chrome is the
/// connection's, because it is a claim about which server answered.
///
/// `@Observable` in its own right. `AppModel` reaches these through forwarding
/// properties of the same names, and a view that reads one still updates,
/// because observation registers whatever was actually read while a body was
/// evaluated — which is the property on this object, not the forwarder that
/// arrived at it.
@Observable
@MainActor
final class Session {
    /// The live connection. Nil before one opens and after one is dropped;
    /// letting go of it is what closes it.
    var db: Database?

    /// One serial queue per session.
    ///
    /// Serial because a single connection cannot service overlapping
    /// statements — that is a property of the connection, which is why the
    /// queue is one too. Two connections have no reason to wait on each other,
    /// and a queue shared between them would be exactly that wait.
    let queue = DispatchQueue(label: "dev.dbclient.core", qos: .userInitiated)

    /// The string this connection was opened with.
    var connString = ""

    var connectionLabel = "Not connected"
    var connectionState: StatusDot.State = .connecting

    /// The colour of the connection now open.
    ///
    /// Carried out of the chooser into the session, because the colour is not there
    /// to decorate the sidebar: somebody marks a connection red so that they can
    /// tell, while looking at a grid of rows, which server they are about to change.
    /// A mark that stopped at the moment of connecting would be a mark shown only
    /// when it does not matter yet.
    var connectionColor: ConnectionColor = .none

    /// What the connection now open is allowed to do.
    ///
    /// Carried out of the chooser into the session for the reason `connectionColor`
    /// above is, and answering a sharper question than the colour does: a mark
    /// that stopped at the moment of connecting would be a mark that applied only
    /// while nothing could go wrong.
    var safety = ConnectionSafety()

    var status = "Connecting…"
    var isBusy = false
    var errorMessage: String?

    /// What the connection's transaction is doing, as of the last thing that
    /// could have changed it.
    ///
    /// Read back from the core after each of those rather than predicted here.
    /// The core is the side that sent the BEGIN, so a copy kept in the window
    /// would be a second answer with nothing to keep it honest — and the one
    /// that is drawn on screen would be the one nobody checked.
    var transaction: TransactionState = .none
}
