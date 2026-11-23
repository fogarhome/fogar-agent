import FogarWire
import SwiftUI

/// Conversación: today's conversation with Fogar, like Claude or Gemini. A text box with a
/// microphone (click to talk and watch the transcript; send it, or stop and edit it), the reply as it arrives,
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
            // Never keep listening out of sight: an open take goes to the box for review.
            conv.vistaDesaparece()
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

/// The turns, newest at the bottom, in a reading column. Follows new text only while the person
/// is at the bottom: scrolled up to read, it stays put. Tokens scroll without animation.
private struct Hilo: View {
    let vista: WireConversation?
    @Environment(\.accessibilityReduceMotion) private var reduce
    /// The bottom marker is on screen (a lazy stack reports it appearing and disappearing).
    @State private var alFinal = true

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
                        .onAppear { alFinal = true }
                        .onDisappear { alFinal = false }
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
            // A new turn: always to the bottom (I just wrote, or Fogar wrote), gently.
            .onChange(of: turnos.count) {
                withAnimation(reduce ? nil : .easeOut(duration: Medida.base)) { proxy.scrollTo("fin", anchor: .bottom) }
            }
            // New text in the last turn: follow it only from the bottom, without animating per token.
            .onChange(of: firma(turnos)) {
                if alFinal { proxy.scrollTo("fin", anchor: .bottom) }
            }
        }
    }

    /// Changes whenever the last turn grows (a token, a progress step, its end).
    private func firma(_ t: [WireTurn]) -> String {
        guard let u = t.last else { return "" }
        return "\(u.turn_id).\(u.reply.utf8.count).\(u.progress?.rawValue ?? "").\(u.outcome?.rawValue ?? "")"
    }
}

private struct Vacio: View {
    var body: some View {
        VStack(spacing: 18) {
            LineaViva(pose: .rest, etiqueta: "Fogar", quieta: true)
                .frame(width: 150)
                .accessibilityHidden(true)
            Text("¿En qué te ayudo?").titularFogar(26)
            Text("Escribe, o pulsa el micrófono y habla: verás el texto mientras hablas.")
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

/// The text box and everything around it: the line about what went wrong, the box itself (the
/// field, or the take while recording) and the permission action when the microphone needs one.
/// Like Claude's box: the microphone starts a take; the take shows what Fogar hears and has two
/// buttons only, stop (the text stays in the box to edit) and send.
private struct Composicion: View {
    let listo: Bool
    var cajaEnfocada: FocusState<Bool>.Binding
    @Environment(Conversacion.self) private var conv
    @Environment(\.accessibilityReduceMotion) private var reduce

    var body: some View {
        let micro = conv.estadoMicro(listo: listo)
        let enToma = conv.enToma
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
            HStack(alignment: .bottom, spacing: 8) {
                contenido
                    .frame(maxWidth: .infinity, alignment: .leading)
                if enToma {
                    SalidasDeLaToma()
                        .transition(cambio)
                } else {
                    BotonMicro(estado: micro)
                    BotonDerecho()
                }
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: Medida.radioVidrio).fill(Color.piedra))
            .overlay {
                RoundedRectangle(cornerRadius: Medida.radioVidrio)
                    .strokeBorder(conv.grabando ? Color.llama.opacity(0.55) : Color.veta, lineWidth: 1)
            }
            .animation(reduce ? .easeOut(duration: Medida.rapido) : .spring(response: 0.32, dampingFraction: 0.86), value: enToma)
            .animation(.easeOut(duration: Medida.base), value: conv.grabando)
            if micro == .pedirPermiso || micro == .denegado {
                HStack(spacing: 10) {
                    if micro == .pedirPermiso {
                        Button { conv.pedirPermisoAhora() } label: { Text("Permitir el micrófono") }
                            .buttonStyle(Pildora(pequena: true, ocupado: conv.pidiendoPermiso))
                            .disabled(conv.pidiendoPermiso)
                    } else {
                        Button { conv.abrirAjustesMicro() } label: { Text("Abrir Ajustes del Sistema") }
                            .buttonStyle(Pildora(pequena: true))
                    }
                    Text(Textos.micro(micro)).meta().fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 4)
            } else if micro == .sinTranscriptor || micro == .desconocido {
                Text(Textos.micro(micro)).meta().padding(.horizontal, 6)
            }
        }
        .frame(maxWidth: 760)
        .frame(maxWidth: .infinity)
        .animation(reduce ? nil : .easeOut(duration: Medida.base), value: lineaArriba?.texto)
    }

    /// Field ⇄ take: a crossfade with a short rise; only the fade under reduce motion.
    private var cambio: AnyTransition {
        reduce ? .opacity : .opacity.combined(with: .offset(y: 4))
    }

    @ViewBuilder private var contenido: some View {
        if conv.enToma {
            TomaVista().transition(cambio)
        } else if conv.vista?.activity == .transcribing && conv.vivo {
            HStack(spacing: 10) {
                Giro().frame(width: 14, height: 14).foregroundStyle(Color.huesoSuave)
                Text("Transcribiendo…").font(Tipo.cuerpo).suave()
            }
            .frame(minHeight: 34)
        } else {
            CampoTexto(cajaEnfocada: cajaEnfocada).transition(cambio)
        }
    }

    private struct Linea { let tono: Tono; let texto: String }

    /// One line at most: the model's own feedback first, then the agent's notice, then the link.
    private var lineaArriba: Linea? {
        if let a = conv.aviso { return Linea(tono: .bad, texto: a) }
        if conv.vivo, let n = conv.vista?.notice { return Linea(tono: .warn, texto: Textos.aviso(n)) }
        if let e = Textos.enlace(conv.link.enlace, listo: listo) { return Linea(tono: .off, texto: e) }
        return nil
    }
}

/// The text field: Return sends, Option-Return adds a line.
private struct CampoTexto: View {
    var cajaEnfocada: FocusState<Bool>.Binding
    @Environment(Conversacion.self) private var conv

    var body: some View {
        @Bindable var conv = conv
        TextField("Escribe a Fogar…", text: $conv.texto, axis: .vertical)
            .textFieldStyle(.plain)
            .font(Tipo.texto(16))
            .foregroundStyle(Color.hueso)
            .lineLimit(1...8)
            .lineSpacing(3)
            .padding(.horizontal, 6)
            .padding(.vertical, 7)
            .focused(cajaEnfocada)
            .onSubmit { conv.enviarAhora() }
            .accessibilityLabel("Mensaje para Fogar")
            .accessibilityHint("Intro envía. Opción e Intro añade una línea.")
    }
}

/// Send, or cancel while Fogar answers.
private struct BotonDerecho: View {
    @Environment(Conversacion.self) private var conv

    var body: some View {
        Group {
            if conv.cancelable {
                BotonRedondo(icono: "stop.fill", etiqueta: "Cancelar", principal: true) {
                    conv.cancelarAhora()
                }
                // ⌘. is handled by the app itself (`Conversacion.tecla`), wherever the focus is.
                .help("Cancelar la respuesta (⌘.)")
                .transition(.scale(scale: 0.8).combined(with: .opacity))
            } else {
                BotonRedondo(icono: "arrow.up", etiqueta: "Enviar", principal: true, ocupado: conv.enviando) {
                    conv.enviarAhora()
                }
                .disabled(!conv.puedeEnviar)
                .help("Enviar (Intro)")
                .transition(.scale(scale: 0.8).combined(with: .opacity))
            }
        }
        .animation(.spring(response: 0.3, dampingFraction: 0.8), value: conv.cancelable)
    }
}

/// The take: «Grabando» only once the agent says the microphone is open (before that, «Abriendo
/// el micrófono…»), and what Fogar understands so far, as it arrives.
private struct TomaVista: View {
    @Environment(Conversacion.self) private var conv

    var body: some View {
        let grabando = conv.grabando
        let parcial = grabando ? conv.vista?.live ?? "" : ""
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                if grabando {
                    Brasa(tam: 8)
                    Text("Grabando").meta()
                } else {
                    Giro().frame(width: 12, height: 12).foregroundStyle(Color.huesoSuave)
                    Text("Abriendo el micrófono…").meta()
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel(grabando ? "Grabando" : "Abriendo el micrófono")
            Text(parcial.isEmpty ? "Te escucho…" : parcial)
                .font(Tipo.texto(16))
                .foregroundStyle(parcial.isEmpty ? Color.huesoTenue : Color.hueso)
                .lineSpacing(3)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentTransition(.opacity)
                .animation(.easeOut(duration: Medida.rapido), value: parcial)
                .accessibilityAddTraits(.updatesFrequently)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 4)
    }
}

/// The two ways out of a take, where the microphone and Send were: stop (the text stays in the
/// box to edit or send, `review`) and send (`send`). Both also work while the take is still
/// opening: it ends as soon as the agent opens it. Esc discards and Return sends (`Conversacion.tecla`).
private struct SalidasDeLaToma: View {
    @Environment(Conversacion.self) private var conv

    var body: some View {
        let cerrando = conv.toma == .cerrando
        HStack(spacing: 8) {
            BotonRedondo(icono: "stop.fill", etiqueta: "Parar y revisar") { conv.parar() }
                .help("Parar: el texto se queda en la caja")
            BotonRedondo(icono: "arrow.up", etiqueta: "Enviar", principal: true) { conv.enviarToma() }
                .help("Enviar (Intro)")
        }
        .disabled(cerrando)
    }
}

/// The microphone: click (or Space, or VoiceOver's action) to start a take. Without permission it
/// asks for it, or opens System Settings when it was denied.
private struct BotonMicro: View {
    let estado: EstadoMicro
    @Environment(Conversacion.self) private var conv
    @State private var hover = false

    var body: some View {
        let usable = estado == .listo || estado == .pedirPermiso || estado == .denegado
        Button(action: accion) {
            Image(systemName: estado == .listo || estado == .pedirPermiso ? "mic" : "mic.slash")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(Color.hueso)
                .frame(width: 34, height: 34)
                .background(Circle().fill(hover && usable ? Color.velo : .clear))
                .contentShape(Circle())
                .opacity(usable ? 1 : 0.42)
        }
        .buttonStyle(Pulsable())
        .onHover { hover = $0 }
        .animation(.easeOut(duration: Medida.rapido), value: hover)
        .help(Textos.micro(estado))
        .accessibilityLabel(estado == .pedirPermiso ? "Permitir el micrófono" : "Hablar")
        .accessibilityHint(Textos.micro(estado))
    }

    private func accion() {
        switch estado {
        case .listo: conv.grabar()
        case .pedirPermiso: conv.pedirPermisoAhora()
        case .denegado: conv.abrirAjustesMicro()
        default: break
        }
    }
}

/// A round icon button for the box (send, cancel, stop).
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
