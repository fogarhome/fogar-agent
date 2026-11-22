import FogarWire
import SwiftUI

/// Conversación: today's conversation with Fogar, like Claude or Gemini. A text box with a
/// microphone (click to record and review, or hold and release to send), the reply as it arrives,
/// what Fogar is doing meanwhile, and a way to cancel. Everything on screen is the agent's view; the
/// app keeps nothing of its own and logs nothing of it.
struct ConversacionView: View {
    @Environment(Conversacion.self) private var conv
    @Environment(AgentLink.self) private var link
    @Environment(\.ventanaVisible) private var ventanaVisible
    @Environment(\.accessibilityReduceMotion) private var reduce
    @FocusState private var cajaEnfocada: Bool
    @State private var aparecida = false

    var body: some View {
        let listo = link.state.status?.phase == .ready
        ZStack(alignment: .bottom) {
            // The fire warms up under the box while Fogar listens: the room's own light, no chrome.
            FondoVivo(calor: conv.grabando ? 0.72 : 0.3, humo: 0.6)
                .frame(height: 320)
                .mask(LinearGradient(colors: [.clear, .black], startPoint: .top, endPoint: .bottom))
                .opacity(0.55)
                .animation(.easeInOut(duration: 1.2), value: conv.grabando)
                .ignoresSafeArea()
            VStack(spacing: 0) {
                Hilo(vista: conv.vista)
                Composicion(listo: listo, cajaEnfocada: $cajaEnfocada)
                    .padding(.horizontal, 32)
                    .padding(.bottom, 24)
                    .padding(.top, 8)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .navigationTitle("Conversación")
        .onAppear {
            aparecida = true
            conv.aLaVista = ventanaVisible
            cajaEnfocada = true
        }
        .onDisappear {
            aparecida = false
            conv.aLaVista = false
        }
        .onChange(of: ventanaVisible) { _, v in conv.aLaVista = aparecida && v }
        .onChange(of: conv.borradorNuevo) { cajaEnfocada = true }
        // Heard as well as seen: an automatic cut, a lost phrase, a refusal.
        .onChange(of: conv.vista?.notice) { _, n in
            if let n { AccessibilityNotification.Announcement(Textos.aviso(n)).post() }
        }
        .onChange(of: conv.aviso) { _, a in
            if let a { AccessibilityNotification.Announcement(a).post() }
        }
        .onChange(of: ultimaRespuesta) { _, r in
            if let r, !r.isEmpty { AccessibilityNotification.Announcement("Fogar: \(r)").post() }
        }
    }

    /// The last finished reply (VoiceOver reads it once, when it is complete).
    private var ultimaRespuesta: String? {
        guard let t = conv.vista?.turns.last, t.outcome == .done else { return nil }
        return t.reply
    }
}

// MARK: El hilo

/// The turns, newest at the bottom, in a reading column. Follows new text unless the person
/// scrolled up to read.
private struct Hilo: View {
    let vista: WireConversation?
    @Environment(\.accessibilityReduceMotion) private var reduce

    var body: some View {
        let turnos = vista?.turns ?? []
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 26) {
                    if turnos.isEmpty {
                        Vacio().padding(.top, 96)
                    }
                    ForEach(Array(turnos.enumerated()), id: \.element.turn_id) { i, t in
                        if i > 0, let a = turnos[i - 1].conversation, let b = t.conversation, a != b {
                            SeparadorDia()
                        }
                        TurnoVista(turno: t, hablando: vista?.speaking == true && i == turnos.count - 1)
                            .transition(reduce ? .opacity : .opacity.combined(with: .offset(y: 10)))
                    }
                    Color.clear.frame(height: 1).id("fin")
                }
                .padding(.horizontal, 32)
                .padding(.top, 28)
                .padding(.bottom, 12)
                .frame(maxWidth: 760)
                .frame(maxWidth: .infinity)
                .animation(reduce ? .easeOut(duration: Medida.rapido) : .spring(response: 0.45, dampingFraction: 0.86),
                           value: turnos.count)
            }
            .defaultScrollAnchor(.bottom)
            .scrollIndicators(.automatic)
            .onChange(of: firma(turnos)) {
                withAnimation(reduce ? nil : .easeOut(duration: Medida.base)) { proxy.scrollTo("fin", anchor: .bottom) }
            }
        }
    }

    /// Changes whenever something new arrives at the bottom (a turn, a token, a progress step).
    private func firma(_ t: [WireTurn]) -> String {
        guard let u = t.last else { return "" }
        return "\(t.count).\(u.turn_id).\(u.reply.utf8.count).\(u.progress?.rawValue ?? "").\(u.outcome?.rawValue ?? "")"
    }
}

private struct Vacio: View {
    var body: some View {
        VStack(spacing: 18) {
            LineaViva(pose: .rest, etiqueta: "Fogar", quieta: true)
                .frame(width: 150)
                .accessibilityHidden(true)
            Text("¿En qué te ayudo?").titularFogar(26)
            Text("Escribe, o pulsa el micrófono para hablar y revisar el texto. Si lo mantienes pulsado, al soltarlo se envía.")
                .font(Tipo.sub).suave()
                .multilineTextAlignment(.center)
                .frame(maxWidth: 440)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity)
        .accessibilityElement(children: .combine)
    }
}

private struct SeparadorDia: View {
    var body: some View {
        HStack(spacing: 12) {
            Rectangle().fill(Color.veta).frame(height: 1)
            Text(Textos.otroDia).meta().fixedSize()
            Rectangle().fill(Color.veta).frame(height: 1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Textos.otroDia)
    }
}

/// One turn: what I said (a bubble on the right), Fogar's reply (plain text on the left, like a
/// page), what it is doing while no text arrives, and how it ended when that is news.
private struct TurnoVista: View {
    let turno: WireTurn
    let hablando: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if turno.source == .push {
                HStack(spacing: 8) {
                    Circle().fill(Color.llama).frame(width: 6, height: 6)
                    Text(Textos.pushEtiqueta).font(Tipo.texto(13, peso: 550)).foregroundStyle(Color.huesoSuave)
                }
                .accessibilityElement(children: .combine)
            }
            if !turno.mine.isEmpty {
                Text(turno.mine)
                    .font(Tipo.cuerpo)
                    .foregroundStyle(Color.hueso)
                    .lineSpacing(3)
                    .textSelection(.enabled)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(RoundedRectangle(cornerRadius: Medida.radioVidrio).fill(Color.piedraAlta))
                    .frame(maxWidth: 560, alignment: .trailing)
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    .accessibilityLabel("Tú: \(turno.mine)")
            }
            VStack(alignment: .leading, spacing: 10) {
                if !turno.reply.isEmpty {
                    Text(turno.reply)
                        .font(Tipo.texto(16))
                        .foregroundStyle(Color.hueso)
                        .lineSpacing(4)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityLabel("Fogar: \(turno.reply)")
                }
                if turno.outcome == nil, let p = turno.progress {
                    HStack(spacing: 10) {
                        Brasa(tam: 8)
                        Text(Textos.progreso(p, herramienta: turno.tool)).meta()
                            .contentTransition(.opacity)
                    }
                    .animation(.easeOut(duration: Medida.base), value: p)
                    .accessibilityElement(children: .combine)
                    .accessibilityAddTraits(.updatesFrequently)
                }
                if hablando {
                    HStack(spacing: 10) {
                        Ondas()
                        Text("Hablando").meta()
                    }
                    .accessibilityElement(children: .combine)
                }
                if let f = Textos.desenlace(turno) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        EstadoIcono(tono: turno.outcome == .cancelled || turno.outcome == .replaced ? .off : .warn, tam: 12)
                        Text(f).meta()
                    }
                    .accessibilityElement(children: .combine)
                }
                if turno.source == .push, turno.audio == .muted {
                    Text(Textos.pushSinSonido).meta()
                }
            }
        }
    }
}

// MARK: La caja

/// The text box and everything around it: the line about what went wrong, the box itself (or the
/// take while recording), and the permission action when the microphone needs one.
private struct Composicion: View {
    let listo: Bool
    var cajaEnfocada: FocusState<Bool>.Binding
    @Environment(Conversacion.self) private var conv
    @Environment(\.accessibilityReduceMotion) private var reduce

    var body: some View {
        let micro = conv.estadoMicro(listo: listo)
        VStack(alignment: .leading, spacing: 10) {
            if let linea = lineaArriba {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    EstadoIcono(tono: linea.tono, tam: 12)
                    Text(linea.texto).font(Tipo.texto(14)).foregroundStyle(Color.hueso)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 6)
                .transition(.opacity)
                .accessibilityElement(children: .combine)
            }
            Group {
                if conv.grabando || conv.toma != .ninguna && conv.toma != .cerrando {
                    TomaVista()
                } else if conv.vista?.activity == .transcribing && conv.vivo {
                    HStack(spacing: 10) {
                        Giro().frame(width: 14, height: 14).foregroundStyle(Color.huesoSuave)
                        Text("Transcribiendo…").font(Tipo.cuerpo).suave()
                        Spacer(minLength: 0)
                    }
                    .frame(minHeight: 34)
                } else {
                    Escribir(micro: micro, cajaEnfocada: cajaEnfocada)
                }
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: Medida.radioVidrio).fill(Color.piedra))
            .overlay {
                RoundedRectangle(cornerRadius: Medida.radioVidrio)
                    .strokeBorder(conv.grabando ? Color.llama.opacity(0.55) : Color.veta, lineWidth: 1)
            }
            .animation(.easeOut(duration: Medida.base), value: conv.grabando)
            if micro == .pedirPermiso || micro == .denegado {
                HStack(spacing: 10) {
                    if micro == .pedirPermiso {
                        Button { Task { await conv.pedirPermiso() } } label: { Text("Permitir el micrófono") }
                            .buttonStyle(Pildora(pequena: true))
                    } else {
                        Button { conv.abrirAjustesMicro() } label: { Text("Abrir Ajustes del Sistema") }
                            .buttonStyle(Pildora(pequena: true))
                    }
                    Text(Textos.micro(micro)).meta().fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 4)
            } else if micro == .sinTranscriptor {
                Text(Textos.micro(micro)).meta().padding(.horizontal, 6)
            }
        }
        .frame(maxWidth: 760)
        .frame(maxWidth: .infinity)
        .animation(reduce ? nil : .easeOut(duration: Medida.base), value: lineaArriba?.texto)
    }

    private struct Linea { let tono: Tono; let texto: String }

    /// One line at most: the model's own feedback first, then the agent's notice, then the link.
    private var lineaArriba: Linea? {
        if let a = conv.aviso { return Linea(tono: .bad, texto: a) }
        if conv.vivo, let n = conv.vista?.notice { return Linea(tono: .warn, texto: Textos.aviso(n)) }
        let listo = self.listo
        if let e = Textos.enlace(conv.link.enlace, listo: listo) { return Linea(tono: .off, texto: e) }
        return nil
    }
}

/// The text box: the field, the microphone, and send (or cancel while Fogar answers).
private struct Escribir: View {
    let micro: EstadoMicro
    var cajaEnfocada: FocusState<Bool>.Binding
    @Environment(Conversacion.self) private var conv

    var body: some View {
        @Bindable var conv = conv
        HStack(alignment: .bottom, spacing: 8) {
            TextField("Escribe a Fogar…", text: $conv.texto, axis: .vertical)
                .textFieldStyle(.plain)
                .font(Tipo.texto(16))
                .foregroundStyle(Color.hueso)
                .lineLimit(1...8)
                .lineSpacing(3)
                .padding(.horizontal, 6)
                .padding(.vertical, 7)
                .focused(cajaEnfocada)
                .onSubmit { Task { await conv.enviar() } }
                .accessibilityLabel("Mensaje para Fogar")
                .accessibilityHint("Intro envía. Opción e Intro añade una línea.")
            BotonMicro(estado: micro)
            if conv.cancelable {
                BotonRedondo(icono: "stop.fill", etiqueta: "Cancelar", principal: true) {
                    Task { await conv.cancelar() }
                }
                .keyboardShortcut(".", modifiers: .command)
                .help("Cancelar la respuesta (⌘.)")
                .transition(.scale(scale: 0.8).combined(with: .opacity))
            } else {
                BotonRedondo(icono: "arrow.up", etiqueta: "Enviar", principal: true, ocupado: conv.enviando) {
                    Task { await conv.enviar() }
                }
                .disabled(!conv.puedeEnviar)
                .help("Enviar (Intro)")
                .transition(.scale(scale: 0.8).combined(with: .opacity))
            }
        }
        .animation(.spring(response: 0.3, dampingFraction: 0.8), value: conv.cancelable)
    }
}

/// The take: a very visible «Grabando», what Fogar understands so far, and the three ways out
/// (or «suelta para enviar» while the button is held).
private struct TomaVista: View {
    @Environment(Conversacion.self) private var conv

    var body: some View {
        let mantener = if case .abierta(true, _, _) = conv.toma { true } else { false }
        let parcial = conv.vista?.live ?? ""
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Brasa(tam: 10)
                Text("Grabando").font(Tipo.texto(14, peso: 600)).foregroundStyle(Color.hueso)
                Spacer(minLength: 0)
                if mantener {
                    Text("Suelta para enviar").meta()
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel(mantener ? "Grabando. Suelta para enviar." : "Grabando")
            Text(parcial.isEmpty ? "Te escucho…" : parcial)
                .font(Tipo.texto(16))
                .foregroundStyle(parcial.isEmpty ? Color.huesoTenue : Color.hueso)
                .lineSpacing(3)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentTransition(.opacity)
                .animation(.easeOut(duration: Medida.rapido), value: parcial)
                .accessibilityAddTraits(.updatesFrequently)
            if !mantener {
                HStack(spacing: 8) {
                    Button { conv.descartar() } label: { Text("Descartar") }
                        .buttonStyle(Pildora(pequena: true))
                        .keyboardShortcut(.cancelAction)
                        .help("Descartar (Esc)")
                    Spacer(minLength: 0)
                    Button { conv.parar() } label: { Text("Parar y revisar") }
                        .buttonStyle(Pildora(pequena: true))
                    Button { conv.enviarYa() } label: { Text("Enviar ya") }
                        .buttonStyle(Pildora(variante: .principal, pequena: true))
                        .keyboardShortcut(.defaultAction)
                        .help("Enviar sin revisar (Intro)")
                }
            }
        }
    }
}

/// The microphone: press to record. A short click leaves the take open (review or send); held
/// past 300 ms, releasing sends. Space or VoiceOver's action start a clicked take.
private struct BotonMicro: View {
    let estado: EstadoMicro
    @Environment(Conversacion.self) private var conv
    @State private var pulsado = false
    @State private var hover = false
    @FocusState private var enfocado: Bool

    var body: some View {
        let usable = estado == .listo || estado == .pedirPermiso || estado == .denegado
        Image(systemName: estado == .listo || estado == .pedirPermiso ? "mic" : "mic.slash")
            .font(.system(size: 15, weight: .medium))
            .foregroundStyle(Color.hueso)
            .frame(width: 34, height: 34)
            .background(Circle().fill(pulsado ? Color.veloAlto : hover && usable ? Color.velo : .clear))
            .overlay { Circle().strokeBorder(Color.hueso, lineWidth: 2).padding(-3).opacity(enfocado ? 1 : 0) }
            .contentShape(Circle())
            .opacity(usable ? 1 : 0.42)
            .scaleEffect(pulsado ? 0.94 : 1)
            .animation(.timingCurve(0.23, 1, 0.32, 1, duration: Medida.pulsar), value: pulsado)
            .onHover { hover = $0 }
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { _ in
                        guard !pulsado, usable else { return }
                        pulsado = true
                        presionar()
                    }
                    .onEnded { _ in
                        guard pulsado else { return }
                        pulsado = false
                        if estado == .listo { conv.soltar() }
                    }
            )
            .focusable(usable)
            .focused($enfocado)
            .focusEffectDisabled()
            .onKeyPress(.space) { accion(); return .handled }
            .help(Textos.micro(estado))
            .accessibilityElement()
            .accessibilityAddTraits(.isButton)
            .accessibilityLabel(estado == .pedirPermiso ? "Permitir el micrófono" : "Micrófono")
            .accessibilityHint(Textos.micro(estado))
            .accessibilityAction { accion() }
    }

    private func presionar() {
        switch estado {
        case .listo:
            conv.pulsar()
            Task {
                try? await Task.sleep(for: .milliseconds(Int(Conversacion.umbralMantener * 1000) + 20))
                if pulsado { conv.comprobarMantener() }
            }
        case .pedirPermiso: Task { await conv.pedirPermiso() }
        case .denegado: conv.abrirAjustesMicro()
        default: break
        }
    }

    /// Keyboard and VoiceOver: no hold, so a clicked take (or stop it to review).
    private func accion() {
        switch estado {
        case .listo: conv.alternar()
        case .pedirPermiso: Task { await conv.pedirPermiso() }
        case .denegado: conv.abrirAjustesMicro()
        default: break
        }
    }
}

/// A round icon button for the box (send, cancel).
private struct BotonRedondo: View {
    let icono: String
    let etiqueta: String
    var principal = false
    var ocupado = false
    let accion: () -> Void
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: accion) {
            ZStack {
                if ocupado {
                    Giro().frame(width: 14, height: 14)
                } else {
                    Image(systemName: icono).font(.system(size: 14, weight: .bold))
                }
            }
            .foregroundStyle(principal ? Color.sobreAccion : Color.hueso)
            .frame(width: 34, height: 34)
            .background(Circle().fill(principal ? Color.accion : Color.velo))
            .contentShape(Circle())
            .opacity(isEnabled ? 1 : 0.32)
        }
        .buttonStyle(Pulsable())
        .accessibilityLabel(etiqueta)
    }
}

/// Presses to 94 % in 120 ms, like the pills.
private struct Pulsable: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.94 : 1)
            .animation(.timingCurve(0.23, 1, 0.32, 1, duration: Medida.pulsar), value: configuration.isPressed)
    }
}

// MARK: Luces

/// An ember that breathes: the «now» of the dashboard (recording, a step in progress). Still under
/// reduce motion and while the window is not on screen.
struct Brasa: View {
    var tam: CGFloat = 8
    @Environment(\.accessibilityReduceMotion) private var reduce
    @Environment(\.ventanaVisible) private var visible
    @State private var respira = false

    var body: some View {
        let quieta = reduce || !visible
        Circle()
            .fill(Color.llama)
            .frame(width: tam, height: tam)
            .background {
                Circle().fill(Color.llama.opacity(0.45))
                    .scaleEffect(quieta ? 1.6 : respira ? 2.3 : 1.1)
                    .opacity(quieta ? 0.5 : respira ? 0 : 0.7)
            }
            .onAppear { arrancar(quieta) }
            .onChange(of: quieta) { _, q in arrancar(q) }
            .accessibilityHidden(true)
    }

    private func arrancar(_ quieta: Bool) {
        guard !quieta else { respira = false; return }
        respira = false
        withAnimation(.easeOut(duration: 1.4).repeatForever(autoreverses: false)) { respira = true }
    }
}

/// Three bars that move while Fogar speaks. Still under reduce motion.
private struct Ondas: View {
    @Environment(\.accessibilityReduceMotion) private var reduce
    @Environment(\.ventanaVisible) private var visible

    var body: some View {
        TimelineView(.animation(minimumInterval: 1 / 20, paused: reduce || !visible)) { tl in
            let t = tl.date.timeIntervalSinceReferenceDate
            HStack(alignment: .center, spacing: 2) {
                ForEach(0..<3, id: \.self) { i in
                    Capsule().fill(Color.llama)
                        .frame(width: 2.5, height: reduce ? 8 : 4 + 8 * abs(sin(t * 5.2 + Double(i) * 1.1)))
                }
            }
            .frame(height: 12)
        }
        .accessibilityHidden(true)
    }
}
