import AppKit
import SwiftUI

/// Fogar Desktop: the main window (sidebar + sections), Settings (⌘,) and the menu bar quick view.
/// One `AgentLink` for all of them: one subscription to the agent, no polling. One
/// `ConversationLink` too, alive as long as the app (the menu bar icon): closing the window does
/// not end the conversation, and my own turns are still confirmed visible (the icon says «hablando»).
@main
struct FogarDesktopApp: App {
    @State private var link: AgentLink
    @State private var conversacion: Conversacion
    @State private var nav = Navegacion()
    /// The agent's LaunchAgent (registered once at first launch; S2 PR 6).
    @State private var servicio = ServicioAgente()
    @AppStorage("apariencia") private var apariencia = Apariencia.sistema

    init() {
        Tipo.registrar()
        let l = AgentLink()
        l.start()
        _link = State(initialValue: l)
        let c = ConversationLink()
        c.start()
        let conv = Conversacion(link: c)
        // ⌘. (and Esc / Return in a take) handled by the app itself, not by a button (#33).
        conv.escucharTeclado()
        _conversacion = State(initialValue: conv)
    }

    var body: some Scene {
        Window("Fogar", id: "main") {
            RootView()
                .environment(link)
                .environment(conversacion)
                .environment(nav)
                .environment(servicio)
        }
        .defaultSize(width: 1100, height: 740)
        .windowStyle(.hiddenTitleBar)
        .commands { FogarCommands(link: link, nav: nav) }

        Settings {
            AjustesView(enVentana: false)
                .environment(link)
                .environment(nav)
                .environment(servicio)
        }
        .windowResizability(.contentSize)

        MenuBarExtra {
            MenuBarView()
                .environment(link)
                .environment(conversacion)
                .environment(nav)
        } label: {
            // The label lives as long as the app: it applies the appearance to every window,
            // the menu bar panel included (preferredColorScheme does not reach that panel).
            MenuBarIcono(modo: IconoModo(link.state, conversacion), titular: Textos.titular(link.state, retirado: link.accesoRetirado).titulo)
                .onChange(of: apariencia, initial: true) { _, a in NSApp.appearance = a.nsAppearance }
                // The agent came back: the conversation connects at once, not at its next backoff.
                .onChange(of: link.state.status != nil) { _, vivo in if vivo { conversacion.link.retryNow() } }
                // First launch: register the agent with launchd; then keep its state current.
                .task { await servicio.arrancar() }
        }
        .menuBarExtraStyle(.window)
    }
}

enum Seccion: String, CaseIterable, Identifiable {
    case conversacion, estado, iniciarSesion, ajustes
    var id: Self { self }

    var titulo: String {
        switch self {
        case .conversacion: "Conversación"
        case .estado: "Estado"
        case .iniciarSesion: "Iniciar sesión"
        case .ajustes: "Ajustes"
        }
    }

    var icono: String {
        switch self {
        case .conversacion: "bubble.left.and.bubble.right"
        case .estado: "point.3.connected.trianglepath.dotted"
        case .iniciarSesion: "person.crop.circle"
        case .ajustes: "gearshape"
        }
    }

    var atajo: KeyEquivalent {
        switch self {
        case .conversacion: "1"
        case .estado: "2"
        case .iniciarSesion: "3"
        case .ajustes: "4"
        }
    }
}

/// The selected section, shared by the window, the menus and the menu bar view. Restored at launch.
@Observable
final class Navegacion {
    var seccion: Seccion {
        didSet { UserDefaults.standard.set(seccion.rawValue, forKey: "seccion") }
    }

    init() {
        seccion = UserDefaults.standard.string(forKey: "seccion").flatMap(Seccion.init(rawValue:)) ?? .estado
    }
}

enum Apariencia: String, CaseIterable, Identifiable {
    case sistema, oscura, clara
    var id: Self { self }
    var nsAppearance: NSAppearance? {
        switch self {
        case .sistema: nil
        case .oscura: NSAppearance(named: .darkAqua)
        case .clara: NSAppearance(named: .aqua)
        }
    }

    var titulo: String {
        switch self {
        case .sistema: "Automática"
        case .oscura: "Oscura"
        case .clara: "Clara"
        }
    }
}

struct FogarCommands: Commands {
    let link: AgentLink
    let nav: Navegacion
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .newItem) {}
        CommandGroup(before: .sidebar) {
            ForEach(Seccion.allCases) { s in
                Button(s.titulo) {
                    nav.seccion = s
                    openWindow(id: "main")
                }
                .keyboardShortcut(s.atajo, modifiers: .command)
            }
            Divider()
            Button("Volver a llamar al agente") { link.retryNow() }
                .keyboardShortcut("r", modifiers: .command)
            Divider()
        }
    }
}
